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
use cpa_core::exec::{ExecError, ExecRequest, ExecResponse, ExecStream, FailureScope, Operation, ResponseBody};
use cpa_core::format::Format;
use futures_util::StreamExt;
use http::HeaderMap;

use crate::codex_oauth::{self, CodexOAuth};
use crate::codex_quota::QuotaSignals;
use crate::codex_request::{self as request, Call, Settings, View};
use crate::codex_response::{self as response, Bootstrap, Processor};
use crate::upstream::{into_response, transport_error};

pub use crate::codex_request::DEFAULT_BASE_URL;

pub struct CodexExecutor {
    client: wreq::Client,
    oauth: CodexOAuth,
    quota: Arc<QuotaSignals>,
    /// Base for OAuth Alpha Search, which Go never derives from credential attributes.
    alpha_base_url: String,
}

/// The production executor. Construction only fails if the TLS backend cannot initialise.
impl Default for CodexExecutor {
    fn default() -> Self {
        Self::new().expect("codex HTTP client")
    }
}

impl CodexExecutor {
    // ponytail: one shared client without the Chrome uTLS profile Go uses for chatgpt.com
    // or per-credential proxies. Swap in the proxy-aware client (crates/cpa-exec/src/proxy.rs,
    // owned by the Claude thread) when it lands.
    pub fn new() -> wreq::Result<Self> {
        let client = wreq::Client::builder()
            .redirect(wreq::redirect::Policy::none())
            .build()?;
        Ok(Self::with_client(client.clone(), CodexOAuth::new(client)))
    }

    /// Caller-built transports. Tests pass a plain client and an OAuth service pointed at
    /// a local mock so nothing reaches OpenAI.
    pub fn with_client(client: wreq::Client, oauth: CodexOAuth) -> Self {
        Self {
            client,
            oauth,
            quota: Arc::default(),
            alpha_base_url: DEFAULT_BASE_URL.into(),
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

    /// Refresh due under Go's 24h Codex lead. Cheap and side-effect free.
    pub fn needs_prepare(&self, credential: &Credential, _cfg: &Config) -> bool {
        codex_oauth::refresh_due(credential, Utc::now())
    }

    /// Refreshes tokens. Go refreshes Codex only in the background and never blocks a
    /// request on it, so a failed refresh while the access token is still valid keeps the
    /// credential usable; the failure is cached for five minutes by the OAuth service.
    pub async fn prepare(&self, credential: &Credential, _cfg: &Config) -> Result<MetadataPatch, ExecError> {
        let now = Utc::now();
        if !codex_oauth::refresh_due(credential, now) {
            return Ok(MetadataPatch::default());
        }
        match self.oauth.refresh_patch(codex_oauth::refresh_token(credential)).await {
            Ok(patch) => Ok(patch),
            Err(error) if codex_oauth::access_usable(credential, now) => {
                tracing::warn!(
                    credential = %credential.id,
                    status = error.status,
                    "codex token refresh failed; current access token is still valid"
                );
                Ok(MetadataPatch::default())
            }
            Err(error) => Err(error),
        }
    }

    pub async fn execute(
        &self,
        credential: &Credential,
        req: ExecRequest,
        cfg: &Config,
    ) -> Result<ExecResponse, ExecError> {
        if req.operation == Operation::CountTokens {
            // ponytail: Go counts Codex input tokens locally with tiktoken
            // (codex_executor_tokens.go); port with the Claude -> Codex translator.
            return Err(ExecError::local(
                501,
                FailureScope::Request,
                "codex token counting is not supported yet",
            ));
        }
        check_response_format(&req)?;
        let settings = Settings::from(cfg);
        let view = View::new(credential);
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

    async fn send(
        &self,
        view: &View<'_>,
        settings: &Settings,
        url: String,
        headers: HeaderMap,
        body: String,
    ) -> Result<ExecResponse, ExecError> {
        let res = self
            .client
            .post(url)
            .redirect(wreq::redirect::Policy::none())
            .headers(headers)
            .body(body)
            .send()
            .await
            .map_err(transport_error)?;
        let status = res.status().as_u16();
        let headers = res.headers().clone();
        self.quota.observe(&view.credential.id, &headers);
        match into_response(res).await {
            Ok(response) => Ok(response),
            Err(error) if !(200..300).contains(&status) => Err(response::status_error(
                status,
                &error.body,
                headers,
                settings.model_level_cooling,
            )),
            Err(error) => Err(error),
        }
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
        let body = request::shape(&req, view, settings, Call::Stream)?;
        let (body, cache) = request::prompt_cache(&req, body, ws_session, false);
        let body = request::sanitize_input_ids(body);
        let headers = request::http_headers(view, settings, &req.headers, &body, &model, cache.as_deref(), true);
        let url = format!("{}/responses", view.base_url);
        let started = Instant::now();
        let res = self.send(view, settings, url, headers, body.clone()).await?;
        let upstream = events(res.body);
        let processor = Processor::new(request::is_native(&req), settings.model_level_cooling);
        let stream = if settings.bootstrap_buffering {
            match response::bootstrap(upstream, processor, settings.bootstrap_timeout, started).await {
                Bootstrap::Reject(error) => return Err(error),
                Bootstrap::Stream(stream) => stream,
            }
        } else {
            response::processed(upstream, processor, 0)
        };
        Ok(ExecResponse {
            status: res.status,
            headers: res.headers,
            body: ResponseBody::Stream(translate_stream(&req, &body, stream)),
        })
    }

    async fn buffered(
        &self,
        view: &View<'_>,
        settings: &Settings,
        req: ExecRequest,
    ) -> Result<ExecResponse, ExecError> {
        let model = request::base_model(&req.model).to_owned();
        let body = request::shape(&req, view, settings, Call::NonStream)?;
        let (body, cache) = request::prompt_cache(&req, body, None, false);
        let body = request::sanitize_input_ids(body);
        let headers = request::http_headers(view, settings, &req.headers, &body, &model, cache.as_deref(), true);
        let url = format!("{}/responses", view.base_url);
        let res = self.send(view, settings, url, headers, body.clone()).await?;
        let mut upstream = events(res.body);
        let mut processor = Processor::new(false, settings.model_level_cooling);
        while let Some(event) = upstream.next().await {
            let Ok(event) = event else {
                break;
            };
            if let Some(completed) = processor.buffered(&event)? {
                return Ok(ExecResponse {
                    status: res.status,
                    headers: res.headers,
                    body: ResponseBody::Buffered(non_stream_output(&req, &body, &completed, Format::Codex)?),
                });
            }
        }
        Err(response::request_scoped(408, response::INCOMPLETE_MESSAGE))
    }

    async fn compact(&self, view: &View<'_>, settings: &Settings, req: ExecRequest) -> Result<ExecResponse, ExecError> {
        let model = request::base_model(&req.model).to_owned();
        let body = request::shape(&req, view, settings, Call::Compact)?;
        let (body, cache) = request::prompt_cache(&req, body, None, false);
        let body = request::sanitize_input_ids(body);
        let headers = request::http_headers(view, settings, &req.headers, &body, &model, cache.as_deref(), false);
        let url = format!("{}/responses/compact", view.base_url);
        let res = self.send(view, settings, url, headers, body.clone()).await?;
        let data = match res.body {
            ResponseBody::Buffered(bytes) => bytes,
            ResponseBody::Stream(mut stream) => {
                let mut out = Vec::new();
                while let Some(event) = stream.next().await {
                    out.extend_from_slice(&event?);
                }
                Bytes::from(out)
            }
        };
        let text = String::from_utf8_lossy(&data);
        Ok(ExecResponse {
            status: res.status,
            headers: res.headers,
            body: ResponseBody::Buffered(non_stream_output(&req, &body, &text, Format::OpenAIResponse)?),
        })
    }

    /// The standalone Codex Alpha Search call (`Server.codexAlphaSearch` minus selection):
    /// already in Codex search format, never translated. The upstream status and body are
    /// returned as is, with only its Content-Type.
    ///
    /// `upstream_model` is the credential-resolved model; it replaces `model` for API keys.
    pub async fn alpha_search(
        &self,
        credential: &Credential,
        body: &[u8],
        client: &HeaderMap,
        upstream_model: &str,
    ) -> Result<ExecResponse, ExecError> {
        let view = View::new(credential);
        let mut body = sanitize_alpha_search(body);
        let url = if view.api_key {
            let base = view.attr("base_url").trim();
            if base.is_empty() {
                return Err(ExecError::local(
                    503,
                    FailureScope::Credential,
                    "Codex Alpha Search API key base URL unavailable",
                ));
            }
            if !upstream_model.trim().is_empty() {
                body = rewrite_alpha_search_model(body, upstream_model.trim());
            }
            format!("{}/alpha/search", base.trim_end_matches('/'))
        } else {
            format!("{}/alpha/search", self.alpha_base_url)
        };
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
        for (name, value) in request::custom_headers(&view, client, None) {
            if let (Ok(name), Ok(value)) = (
                http::HeaderName::try_from(name.as_str()),
                http::HeaderValue::from_str(&value),
            ) {
                headers.insert(name, value);
            }
        }
        headers.insert("accept-encoding", http::HeaderValue::from_static("gzip"));
        let res = self
            .client
            .post(url)
            .redirect(wreq::redirect::Policy::none())
            .headers(headers)
            .body(body)
            .send()
            .await
            .map_err(transport_error)?;
        let status = res.status().as_u16();
        let content_type = res.headers().get(http::header::CONTENT_TYPE).cloned();
        let data = match into_response(res).await {
            Ok(ExecResponse {
                body: ResponseBody::Buffered(bytes),
                ..
            }) => bytes,
            Ok(ExecResponse {
                body: ResponseBody::Stream(mut stream),
                ..
            }) => {
                let mut out = Vec::new();
                while let Some(chunk) = stream.next().await {
                    out.extend_from_slice(&chunk?);
                    if out.len() > 32 << 20 {
                        out.truncate(32 << 20);
                        break;
                    }
                }
                Bytes::from(out)
            }
            // ponytail: upstream error bodies are read through the shared 64 KiB bound,
            // not Go's 32 MiB cap.
            Err(error) if !(200..300).contains(&status) => error.body,
            Err(error) => return Err(error),
        };
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
fn events(body: ResponseBody) -> ExecStream {
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

/// The client body for a buffered response: the registered pair, else Go's identity
/// for Codex clients and the Responses object for OpenAI Responses clients.
fn non_stream_output(
    req: &ExecRequest,
    translated: &str,
    completed: &str,
    upstream: Format,
) -> Result<Bytes, ExecError> {
    let pair = cpa_translate::pair(req.response_format, upstream);
    let out = match pair {
        Some(pair) => {
            let ctx = cpa_translate::ResponseCtx {
                model: &req.model,
                original_request: &req.original_body,
                translated_request: translated.as_bytes(),
            };
            String::from_utf8_lossy(&(pair.non_stream)(&ctx, completed.as_bytes()).map_err(translate_error)?)
                .into_owned()
        }
        // ponytail: mirrors ConvertCodexResponseToOpenAIResponsesNonStream until the
        // OpenAI Responses <- Codex pair is registered in cpa-translate.
        None if req.response_format == Format::OpenAIResponse && upstream == Format::Codex => {
            let root = gjson::parse(completed);
            match root.get("type").str() {
                "" if root.get("output").kind() == gjson::Kind::Array => completed.to_owned(),
                "response.completed" | "response.incomplete" => root.get("response").json().to_owned(),
                _ => String::new(),
            }
        }
        None => completed.to_owned(),
    };
    let out = if req.response_format == Format::OpenAIResponse {
        response::ensure_usage_details(out)
    } else {
        out
    };
    Ok(Bytes::from(out))
}

/// Streaming translation for non-Codex clients; identity for Codex/Responses clients.
fn translate_stream(req: &ExecRequest, translated: &str, upstream: ExecStream) -> ExecStream {
    let Some(pair) = cpa_translate::pair(req.response_format, Format::Codex) else {
        return upstream;
    };
    let ctx = cpa_translate::ResponseCtx {
        model: &req.model,
        original_request: &req.original_body,
        translated_request: translated.as_bytes(),
    };
    let translator = (pair.stream)(&ctx);
    struct State {
        upstream: ExecStream,
        translator: Box<dyn cpa_translate::StreamTranslator>,
        ready: std::collections::VecDeque<Bytes>,
        done: bool,
    }
    futures_util::stream::unfold(
        State {
            upstream,
            translator,
            ready: Default::default(),
            done: false,
        },
        |mut st| async move {
            loop {
                if let Some(event) = st.ready.pop_front() {
                    return Some((Ok(event), st));
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
                    Ok(events) => st.ready.extend(events),
                    Err(error) => {
                        st.done = true;
                        return Some((Err(error), st));
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
