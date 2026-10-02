//! Kimi executor (internal/runtime/executor/kimi_executor.go).
//!
//! Three wire paths, chosen by the client format:
//! - Claude Messages: delegated to the Claude executor against Kimi's Anthropic-compatible
//!   base, with model normalization, response-model restoration and native reasoning
//!   replay (kimi_replay).
//! - OpenAI Responses: native `/v1/responses`, body kept in Responses shape.
//! - Everything else: translated to OpenAI Chat Completions at `/v1/chat/completions`.
//!
//! Shared stages go through adapters named after their owners: thinking (kimi_thinking),
//! custom headers and proxies (kimi_http), payload rules and Codex-client rewrites (below).
//! ponytail: the apply_patch Responses bridge (translator common) is not applied; requests
//! without an apply_patch custom tool are unaffected.

use std::collections::VecDeque;
use std::sync::Arc;

use bytes::Bytes;
use cpa_core::config::Config;
use cpa_core::credential::{Credential, MetadataPatch};
use cpa_core::exec::{ExecError, ExecRequest, ExecResponse, ExecStream, FailureScope, Operation, ResponseBody};
use cpa_core::format::Format;
use cpa_translate::{RequestCtx, ResponseCtx, StreamTranslator};
use futures_util::StreamExt;

use crate::claude::ClaudeExecutor;
use crate::kimi_auth::{self, DeviceFlow};
use crate::kimi_http::{
    BUILD_VERSION, Clients, GoHeaders, Upstream, custom_headers, default_client, go_arch, go_os, hostname, lines,
    proxy_url, read_all, refresh_due, rfc3339_local_now, send, status_error,
};
use crate::kimi_json::{GoValue, delete, gstr, join_array, set_raw, set_str, valid};
use crate::kimi_replay::{self, ReplayCache};
use crate::kimi_thinking::{self, parse_suffix};

const REASONING_UNAVAILABLE: &str = "[reasoning unavailable]";

/// Provider strings served by this executor.
pub const PROVIDERS: [&str; 4] = ["kimi", "kimi-ai", "kimi.ai", "kimi.com"];

/// Claude-format traffic goes through the shared [`ClaudeExecutor`] passed to
/// [`KimiExecutor::execute`]; Go embeds one in the Kimi executor for the same purpose.
pub struct KimiExecutor {
    clients: Clients,
    oauth_host: Option<String>,
    replay: Arc<ReplayCache>,
}

impl Default for KimiExecutor {
    fn default() -> Self {
        Self::with_client(default_client())
    }
}

impl KimiExecutor {
    /// Uses a caller-built client (tests point it at local mocks).
    pub fn with_client(client: wreq::Client) -> Self {
        Self {
            clients: Clients::new(client),
            oauth_host: None,
            replay: Arc::default(),
        }
    }

    /// Sends token refreshes to a local mock instead of auth.kimi.com / auth.kimi.ai.
    pub fn with_oauth_host(mut self, host: &str) -> Self {
        self.oauth_host = Some(host.to_owned());
        self
    }

    pub fn needs_prepare(&self, credential: &Credential, _cfg: &Config) -> bool {
        refresh_token(credential).is_some()
            && refresh_due(
                credential,
                chrono::Duration::from_std(kimi_auth::REFRESH_LEAD).ok(),
                chrono::Utc::now(),
            )
    }

    /// Refresh grant; Kimi has no other preparation.
    pub async fn prepare(&self, credential: &Credential, cfg: &Config) -> Result<MetadataPatch, ExecError> {
        let Some(refresh) = refresh_token(credential) else {
            return Ok(MetadataPatch::default());
        };
        let domain = kimi_auth::resolve_domain(credential);
        let device_id = credential.str("device_id").unwrap_or_default();
        let mut flow = DeviceFlow::new(self.clients.get(&proxy_url(credential, cfg)), domain, device_id);
        if let Some(host) = &self.oauth_host {
            flow = flow.with_oauth_host(host);
        }
        let tokens = flow.refresh(refresh).await?;
        Ok(kimi_auth::refresh_patch(
            credential,
            &tokens,
            &base_url(credential),
            rfc3339_local_now(),
        ))
    }

    pub async fn execute(
        &self,
        claude: &ClaudeExecutor,
        credential: &Credential,
        req: ExecRequest,
        cfg: &Config,
    ) -> Result<ExecResponse, ExecError> {
        if req.operation == Operation::CountTokens || req.source_format == Format::Claude {
            return self.execute_claude(claude, credential, req, cfg).await;
        }
        let client = self.clients.get(&proxy_url(credential, cfg));
        if req.source_format == Format::OpenAIResponse {
            return execute_responses(&client, credential, req, cfg).await;
        }
        execute_chat(&client, credential, req, cfg).await
    }

    async fn execute_claude(
        &self,
        claude: &ClaudeExecutor,
        credential: &Credential,
        mut req: ExecRequest,
        cfg: &Config,
    ) -> Result<ExecResponse, ExecError> {
        let mut delegated = credential.clone();
        delegated
            .attributes
            .insert("base_url".into(), claude_base_url(credential));
        let client_model = req.model.clone();
        let upstream_model = normalize_upstream_model(parse_suffix(&req.model).0);
        // ponytail: Go's ClaudeExecutor applies the normalizer itself and forces upstream
        // count_tokens for Kimi. The Rust Claude executor has neither hook yet, so the
        // model is rewritten here and count_tokens follows its local estimate.
        if let Ok(text) = std::str::from_utf8(&req.body)
            && valid(text)
            && let Ok(updated) = set_str(text, "model", &upstream_model)
        {
            req.body = Bytes::from(updated);
        }
        if req.operation == Operation::CountTokens {
            return claude.execute(&delegated, req, cfg).await;
        }
        let scope = kimi_replay::prepare(&self.replay, &mut req);
        let streaming = req.stream;
        let mut response = match claude.execute(&delegated, req, cfg).await {
            Ok(response) => response,
            Err(error) => {
                if scope.applied && kimi_replay::clears_after(&error) {
                    scope.clear();
                }
                return Err(error);
            }
        };
        response.body = match response.body {
            ResponseBody::Buffered(body) => {
                let body = restore_response_model(&body, &client_model);
                if !streaming {
                    scope.store_response(&body);
                }
                ResponseBody::Buffered(body)
            }
            ResponseBody::Stream(stream) => {
                let restored = stream
                    .map(move |event| event.map(|e| restore_response_model(&e, &client_model)))
                    .boxed();
                ResponseBody::Stream(kimi_replay::wrap_stream(restored, scope))
            }
        };
        Ok(response)
    }
}

fn refresh_token(credential: &Credential) -> Option<&str> {
    credential.str("refresh_token").filter(|s| !s.trim().is_empty())
}

/// `ResolveKimiBaseURL`: attribute, metadata, then the domain default.
pub(crate) fn base_url(credential: &Credential) -> String {
    let attr = credential
        .attributes
        .get("base_url")
        .map(|s| s.trim().trim_end_matches('/'))
        .unwrap_or_default();
    if !attr.is_empty() {
        return attr.to_owned();
    }
    let meta = credential.str("base_url").map(str::trim).unwrap_or_default();
    if !meta.is_empty() {
        return meta.trim_end_matches('/').to_owned();
    }
    kimi_auth::api_base(kimi_auth::resolve_domain(credential)).to_owned()
}

fn chat_url(credential: &Credential) -> String {
    let base = base_url(credential);
    if base.ends_with("/v1") {
        format!("{base}/chat/completions")
    } else {
        format!("{base}/v1/chat/completions")
    }
}

fn responses_url(credential: &Credential) -> String {
    let base = base_url(credential);
    if base.ends_with("/v1") {
        format!("{base}/responses")
    } else {
        format!("{base}/v1/responses")
    }
}

fn claude_base_url(credential: &Credential) -> String {
    let base = base_url(credential);
    base.strip_suffix("/v1").unwrap_or(&base).to_owned()
}

/// `kimiCreds`: metadata access token, then attribute access token or API key.
fn access_token(credential: &Credential) -> String {
    if let Some(token) = credential.str("access_token").filter(|s| !s.trim().is_empty()) {
        return token.to_owned();
    }
    ["access_token", "api_key"]
        .iter()
        .find_map(|k| credential.attributes.get(*k).filter(|s| !s.is_empty()))
        .cloned()
        .unwrap_or_default()
}

/// The executor's device identity: credential `device_id`, else kimi-cli's stored ID.
fn device_id(credential: &Credential) -> String {
    if let Some(id) = credential.str("device_id").map(str::trim).filter(|s| !s.is_empty()) {
        return id.to_owned();
    }
    let Some(home) = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE")) else {
        return "cli-proxy-api-device".into();
    };
    let home = std::path::PathBuf::from(home);
    let dir = match go_os() {
        "darwin" => home.join("Library/Application Support/kimi"),
        "windows" => std::env::var_os("APPDATA")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| home.join("AppData/Roaming"))
            .join("kimi"),
        _ => home.join(".local/share/kimi"),
    };
    std::fs::read_to_string(dir.join("device_id"))
        .map(|s| s.trim().to_owned())
        .unwrap_or_else(|_| "cli-proxy-api-device".into())
}

/// `applyKimiHeadersWithAuth` plus credential custom headers.
fn headers(credential: &Credential, req: &ExecRequest, stream: bool) -> GoHeaders {
    let mut h = GoHeaders::new();
    h.set("Content-Type", "application/json");
    h.set("Authorization", format!("Bearer {}", access_token(credential)));
    h.set("User-Agent", format!("CLIProxyAPI/{BUILD_VERSION}"));
    h.set("X-Msh-Platform", "CLIProxyAPI");
    h.set("X-Msh-Version", BUILD_VERSION);
    h.set("X-Msh-Device-Name", hostname().unwrap_or_else(|| "unknown".into()));
    h.set("X-Msh-Device-Model", format!("{} {}", go_os(), go_arch()));
    h.set("X-Msh-Device-Id", device_id(credential));
    h.set(
        "Accept",
        if stream {
            "text/event-stream"
        } else {
            "application/json"
        },
    );
    for (name, value) in custom_headers(credential, &req.headers, req.session.as_deref()) {
        h.set(&name, value);
    }
    h
}

fn request_error(message: impl Into<String>) -> ExecError {
    ExecError::local(400, FailureScope::Request, message)
}

/// A plain Go error from the executor: the handler answers 500 with its text.
fn internal_error(message: impl Into<String>) -> ExecError {
    ExecError::local(500, FailureScope::Request, message)
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
/// `requests.payload` rules here (ApplyPayloadConfigWithRequest) to the final provider body;
/// identity until the shared module lands. `target` and `source` are Go format names.
fn apply_payload_rules(
    _cfg: &Config,
    _model: &str,
    _target: &str,
    _source: &str,
    body: String,
    _req: &ExecRequest,
) -> String {
    body
}

/// ponytail: adapter for `cpa_common::codex_client` (owner: Codex thread). Go wraps request
/// translation in TranslateRequestWithCodexMultiAgentV2, which rewrites Codex CLI requests
/// (integer tool schemas, multi-agent v2 input); identity until the shared module lands.
fn codex_client_request(_req: &ExecRequest, body: &[u8]) -> Vec<u8> {
    body.to_vec()
}

/// Go's bufio.Scanner limits: 1 MiB for Chat Completions lines, 50 MiB for Responses.
const CHAT_LINE_LIMIT: usize = 1_048_576;
const RESPONSES_LINE_LIMIT: usize = 52_428_800;

async fn post(client: &wreq::Client, url: &str, headers: GoHeaders, body: String) -> Result<Upstream, ExecError> {
    let upstream = send(client, url, headers, body, None).await?;
    if !(200..300).contains(&upstream.status) {
        return Err(status_error(upstream).await);
    }
    Ok(upstream)
}

/// Chat Completions path (Execute / ExecuteStream for non-Claude, non-Responses clients).
async fn execute_chat(
    client: &wreq::Client,
    credential: &Credential,
    req: ExecRequest,
    cfg: &Config,
) -> Result<ExecResponse, ExecError> {
    let (base_model, _) = parse_suffix(&req.model);
    let request_pair = cpa_translate::pair(req.source_format, Format::OpenAI);
    let response_pair = cpa_translate::pair(req.response_format, Format::OpenAI);
    if request_pair.is_none() && req.source_format != Format::OpenAI {
        return Err(not_registered("Kimi request"));
    }
    let Some(response_pair) = response_pair else {
        return Err(not_registered("Kimi response"));
    };
    let translated = match request_pair {
        Some(pair) => (pair.request)(
            &RequestCtx {
                model: base_model,
                stream: req.stream,
            },
            &codex_client_request(&req, &req.body),
        )
        .map_err(|e| request_error(e.to_string()))?,
        None => codex_client_request(&req, &req.body),
    };
    let translated = String::from_utf8(translated).map_err(|_| request_error("translated body is not UTF-8"))?;
    let upstream_model = normalize_upstream_model(base_model);
    let mut body = set_str(&translated, "model", &upstream_model)
        .map_err(|e| internal_error(format!("kimi executor: failed to set model in payload: {e}")))?;
    body = kimi_thinking::apply(
        &body,
        text(&req.body)?,
        text(&req.original_body)?,
        &req.model,
        req.source_format.as_str(),
        "kimi",
        "kimi",
    )
    .map_err(|e| request_error(e.0))?;
    if req.stream {
        body = set_raw(&body, "stream_options.include_usage", "true")
            .map_err(|e| internal_error(format!("kimi executor: failed to set stream_options in payload: {e}")))?;
    }
    body = apply_payload_rules(cfg, base_model, "openai", req.source_format.as_str(), body, &req);
    body = normalize_tool_message_links(&body)?;
    body = normalize_tools(&body);
    body = normalize_temperature(&body);
    let upstream = post(
        client,
        &chat_url(credential),
        headers(credential, &req, req.stream),
        body.clone(),
    )
    .await?;
    let translated = Bytes::from(body);
    let ctx = ResponseCtx {
        model: &req.model,
        original_request: &req.original_body,
        translated_request: &translated,
    };
    let body = if req.stream {
        ResponseBody::Stream(translate_lines(
            lines(upstream.body, CHAT_LINE_LIMIT),
            (response_pair.stream)(&ctx),
        ))
    } else {
        let data = read_all(upstream.body, usize::MAX, false).await?;
        let out = (response_pair.non_stream)(&ctx, &data)
            .map_err(|e| ExecError::local(502, FailureScope::Request, e.to_string()))?;
        ResponseBody::Buffered(Bytes::from(out))
    };
    Ok(ExecResponse {
        status: upstream.status,
        headers: upstream.headers,
        body,
    })
}

/// Go's per-line stream loop: each non-empty scanned line goes through the translator;
/// after the last line (or a scan error) the translator flushes, then the error follows.
fn translate_lines(upstream: ExecStream, translator: Box<dyn StreamTranslator>) -> ExecStream {
    struct State {
        upstream: ExecStream,
        translator: Box<dyn StreamTranslator>,
        ready: VecDeque<Result<Bytes, ExecError>>,
        done: bool,
    }
    let fail = |error: cpa_translate::Error| ExecError::local(502, FailureScope::Request, error.to_string());
    futures_util::stream::unfold(
        State {
            upstream,
            translator,
            ready: VecDeque::new(),
            done: false,
        },
        move |mut st| async move {
            loop {
                if let Some(item) = st.ready.pop_front() {
                    return Some((item, st));
                }
                if st.done {
                    return None;
                }
                match st.upstream.next().await {
                    Some(Ok(line)) if line.is_empty() => {}
                    Some(Ok(line)) => match st.translator.event(&line) {
                        Ok(events) => st.ready.extend(events.into_iter().map(Ok)),
                        Err(error) => {
                            st.done = true;
                            st.ready.push_back(Err(fail(error)));
                        }
                    },
                    end => {
                        st.done = true;
                        match st.translator.finish() {
                            Ok(events) => st.ready.extend(events.into_iter().map(Ok)),
                            Err(error) => st.ready.push_back(Err(fail(error))),
                        }
                        if let Some(Err(error)) = end {
                            st.ready.push_back(Err(error));
                        }
                    }
                }
            }
        },
    )
    .boxed()
}

/// Native Responses streaming: Go writes every scanned line plus `\n` as one chunk and
/// the Responses route joins chunks into frames (responses_frames), flushing what is
/// pending at the end and before a terminal error.
fn responses_frames(lines: ExecStream) -> ExecStream {
    struct State {
        lines: ExecStream,
        joiner: crate::responses_frames::Joiner,
        ready: VecDeque<Result<Bytes, ExecError>>,
        done: bool,
    }
    futures_util::stream::unfold(
        State {
            lines,
            joiner: Default::default(),
            ready: VecDeque::new(),
            done: false,
        },
        |mut st| async move {
            loop {
                if let Some(item) = st.ready.pop_front() {
                    return Some((item, st));
                }
                if st.done {
                    return None;
                }
                match st.lines.next().await {
                    Some(Ok(line)) => {
                        let mut chunk = line.to_vec();
                        chunk.push(b'\n');
                        let frames = st.joiner.write(&chunk);
                        st.ready.extend(frames.into_iter().map(Ok));
                    }
                    end => {
                        st.done = true;
                        let frames = st.joiner.flush();
                        st.ready.extend(frames.into_iter().map(Ok));
                        if let Some(Err(error)) = end {
                            st.ready.push_back(Err(error));
                        }
                    }
                }
            }
        },
    )
    .boxed()
}

/// `SetBoolIfDifferent`.
fn set_bool_if_different(body: &str, path: &str, value: bool) -> String {
    let current = gjson::get(body, path);
    let same = matches!(
        (current.kind(), value),
        (gjson::Kind::True, true) | (gjson::Kind::False, false)
    );
    if same {
        return body.to_owned();
    }
    set_raw(body, path, if value { "true" } else { "false" }).unwrap_or_else(|_| body.to_owned())
}

/// Native Responses path (executeResponses / executeResponsesStream).
async fn execute_responses(
    client: &wreq::Client,
    credential: &Credential,
    req: ExecRequest,
    cfg: &Config,
) -> Result<ExecResponse, ExecError> {
    if req.alt.as_deref() == Some("responses/compact") {
        return Err(if req.stream {
            ExecError::local(
                400,
                FailureScope::Request,
                "streaming not supported for /responses/compact",
            )
        } else {
            ExecError::local(501, FailureScope::Request, "/responses/compact not supported")
        });
    }
    let response_pair = cpa_translate::pair(req.response_format, Format::OpenAIResponse);
    if response_pair.is_none() && req.response_format != Format::OpenAIResponse {
        return Err(not_registered("Kimi Responses"));
    }
    let (base_model, _) = parse_suffix(&req.model);
    let upstream_model = normalize_upstream_model(base_model);
    let source = text(&req.body)?;
    let mut body = set_str(source, "model", &upstream_model)
        .map_err(|e| internal_error(format!("kimi executor: failed to set model in payload: {e}")))?;
    body = set_bool_if_different(&body, "stream", req.stream);
    body = kimi_thinking::apply(
        &body,
        source,
        text(&req.original_body)?,
        &req.model,
        req.source_format.as_str(),
        "codex",
        "kimi",
    )
    .map_err(|e| request_error(e.0))?;
    body = apply_payload_rules(
        cfg,
        base_model,
        "openai-response",
        req.source_format.as_str(),
        body,
        &req,
    );
    body = normalize_responses_input(&body);
    body = normalize_tools(&body);
    body = normalize_temperature(&body);
    let upstream = post(
        client,
        &responses_url(credential),
        headers(credential, &req, req.stream),
        body.clone(),
    )
    .await?;
    let translated = Bytes::from(body);
    let ctx = ResponseCtx {
        model: &req.model,
        original_request: &req.original_body,
        translated_request: &translated,
    };
    let out = match (req.stream, response_pair) {
        (true, Some(pair)) => ResponseBody::Stream(translate_lines(
            lines(upstream.body, RESPONSES_LINE_LIMIT),
            (pair.stream)(&ctx),
        )),
        // Native Responses clients get every scanned line back with "\n" appended.
        (true, None) => ResponseBody::Stream(responses_frames(lines(upstream.body, RESPONSES_LINE_LIMIT))),
        (false, pair) => {
            let data = read_all(upstream.body, usize::MAX, false).await?;
            match pair {
                Some(pair) => ResponseBody::Buffered(Bytes::from(
                    (pair.non_stream)(&ctx, &data)
                        .map_err(|e| ExecError::local(502, FailureScope::Request, e.to_string()))?,
                )),
                None => ResponseBody::Buffered(data),
            }
        }
    };
    Ok(ExecResponse {
        status: upstream.status,
        headers: upstream.headers,
        body: out,
    })
}

/// `normalizeKimiUpstreamModel`: strip `kimi-` and `[1m]`, map K2.7/K2.8 Code aliases,
/// keep a thinking suffix.
pub(crate) fn normalize_upstream_model(model: &str) -> String {
    let model = model.trim();
    let (name, suffix) = parse_suffix(model);
    let mut base = name.trim().to_ascii_lowercase();
    if let Some(stripped) = base.strip_suffix("[1m]") {
        base = stripped.to_owned();
    }
    let normalized = match base.as_str() {
        "kimi-k2.8" | "k2.8" | "kimi-k2.8-code" | "k2.8-code" | "kimi-k2.8-preview" | "k2.8-preview"
        | "kimi-k2.7-code" | "k2.7-code" | "kimi-for-coding" | "for-coding" => "kimi-for-coding".to_owned(),
        "kimi-k2.7-code-highspeed" | "k2.7-code-highspeed" | "kimi-for-coding-highspeed" | "for-coding-highspeed" => {
            "kimi-for-coding-highspeed".to_owned()
        }
        _ => base.trim().strip_prefix("kimi-").unwrap_or(base.trim()).to_owned(),
    };
    match suffix {
        Some(raw) => format!("{normalized}({raw})"),
        None => normalized,
    }
}

fn usable_reasoning(reasoning: &str) -> bool {
    let trimmed = reasoning.trim();
    !trimmed.is_empty() && trimmed != REASONING_UNAVAILABLE
}

fn content_part_empty(part: &gjson::Value<'_>) -> bool {
    match part.kind() {
        gjson::Kind::Null => true,
        gjson::Kind::String => gstr(part).trim().is_empty(),
        gjson::Kind::Object => {
            let text = part.get("text");
            if text.exists() {
                return gstr(&text).trim().is_empty();
            }
            gstr(&part.get("type")).trim() == "text" || part.json().trim() == "{}"
        }
        _ => false,
    }
}

fn should_drop_assistant(msg: &gjson::Value<'_>) -> bool {
    if gstr(&msg.get("role")).trim() != "assistant" {
        return false;
    }
    let tool_calls = msg.get("tool_calls");
    let has_tool_calls = tool_calls.kind() == gjson::Kind::Array && !tool_calls.array().is_empty();
    let call = msg.get("function_call");
    let has_function_call = call.exists()
        && call.kind() != gjson::Kind::Null
        && !(call.kind() == gjson::Kind::Object && call.json().trim() == "{}");
    let has_reasoning = {
        let r = msg.get("reasoning_content");
        r.exists() && !gstr(&r).trim().is_empty()
    };
    if has_tool_calls || has_function_call || has_reasoning {
        return false;
    }
    let content = msg.get("content");
    match content.kind() {
        gjson::Kind::Null => true,
        _ if !content.exists() => true,
        gjson::Kind::String => gstr(&content).trim().is_empty(),
        gjson::Kind::Array => content.array().iter().all(content_part_empty),
        _ => false,
    }
}

fn fallback_reasoning(msg: &gjson::Value<'_>, latest: Option<&str>) -> String {
    if let Some(latest) = latest.filter(|l| usable_reasoning(l)) {
        return latest.to_owned();
    }
    let content = msg.get("content");
    if content.kind() == gjson::Kind::String && !gstr(&content).trim().is_empty() {
        return gstr(&content).trim().to_owned();
    }
    if content.kind() == gjson::Kind::Array {
        let parts: Vec<String> = content
            .array()
            .iter()
            .map(|item| gstr(&item.get("text")).trim().to_owned())
            .filter(|t| !t.is_empty())
            .collect();
        if !parts.is_empty() {
            return parts.join("\n");
        }
    }
    REASONING_UNAVAILABLE.to_owned()
}

/// `normalizeKimiToolMessageLinks`: drop empty assistant turns, repair tool_call_id links
/// and give tool-calling assistant turns reasoning_content.
fn normalize_tool_message_links(body: &str) -> Result<String, ExecError> {
    if body.is_empty() || !valid(body) {
        return Ok(body.to_owned());
    }
    let messages = gjson::get(body, "messages");
    if messages.kind() != gjson::Kind::Array {
        return Ok(body.to_owned());
    }
    let msgs = messages.array();
    let mut dropped = vec![false; msgs.len()];
    let mut patches: Vec<(usize, &str, String, &str)> = Vec::new();
    let mut pending: Vec<String> = Vec::new();
    let mut latest: Option<String> = None;
    for (index, msg) in msgs.iter().enumerate() {
        if should_drop_assistant(msg) {
            dropped[index] = true;
            continue;
        }
        match gstr(&msg.get("role")).trim() {
            "assistant" => {
                let reasoning = msg.get("reasoning_content");
                if reasoning.exists() && usable_reasoning(&gstr(&reasoning)) {
                    latest = Some(gstr(&reasoning).to_owned());
                }
                let calls = msg.get("tool_calls");
                if calls.kind() == gjson::Kind::Array && !calls.array().is_empty() {
                    if !reasoning.exists() || !usable_reasoning(&gstr(&reasoning)) {
                        patches.push((
                            index,
                            "reasoning_content",
                            fallback_reasoning(msg, latest.as_deref()),
                            "failed to set assistant reasoning_content",
                        ));
                    }
                    for call in calls.array() {
                        let id = gstr(&call.get("id")).trim().to_owned();
                        if !id.is_empty() {
                            pending.push(id);
                        }
                    }
                }
            }
            "tool" => {
                let mut id = gstr(&msg.get("tool_call_id")).trim().to_owned();
                if id.is_empty() {
                    id = gstr(&msg.get("call_id")).trim().to_owned();
                    if !id.is_empty() {
                        patches.push((
                            index,
                            "tool_call_id",
                            id.clone(),
                            "failed to set tool_call_id from call_id",
                        ));
                    }
                }
                if id.is_empty() && pending.len() == 1 {
                    id = pending[0].clone();
                    patches.push((index, "tool_call_id", id.clone(), "failed to infer tool_call_id"));
                }
                if let Some(pos) = pending.iter().position(|p| *p == id).filter(|_| !id.is_empty()) {
                    pending.remove(pos);
                }
            }
            _ => {}
        }
    }
    let any_dropped = dropped.iter().any(|d| *d);
    if !any_dropped && patches.is_empty() {
        return Ok(body.to_owned());
    }
    if !any_dropped && patches.len() == 1 {
        let (index, path, value, context) = &patches[0];
        return set_str(body, &format!("messages.{index}.{path}"), value)
            .map_err(|e| internal_error(format!("kimi executor: {context}: {e}")));
    }
    let mut items = Vec::with_capacity(msgs.len());
    let mut next = patches.iter().peekable();
    for (index, msg) in msgs.iter().enumerate() {
        if dropped[index] {
            continue;
        }
        let mut raw = msg.json().to_owned();
        while let Some((_, path, value, context)) = next.next_if(|p| p.0 == index) {
            raw = set_str(&raw, path, value).map_err(|e| internal_error(format!("kimi executor: {context}: {e}")))?;
        }
        items.push(raw);
    }
    set_raw(body, "messages", &join_array(&items)).map_err(|e| {
        let context = if any_dropped {
            "failed to drop empty assistant messages"
        } else {
            patches[0].3
        };
        internal_error(format!("kimi executor: {context}: {e}"))
    })
}

/// `normalizeKimiTools`: inline local `$ref`s and default the root schema type.
fn normalize_tools(body: &str) -> String {
    if body.is_empty() {
        return body.to_owned();
    }
    let body = normalize_tool_list(body, "tools", true);
    normalize_tool_list(&body, "functions", false)
}

fn normalize_tool_list(body: &str, key: &str, is_tools: bool) -> String {
    let items = gjson::get(body, key);
    if items.kind() != gjson::Kind::Array {
        return body.to_owned();
    }
    let items = items.array();
    if items.is_empty() {
        return body.to_owned();
    }
    let mut changed = false;
    let mut updated = Vec::with_capacity(items.len());
    for item in &items {
        let mut raw = item.json().to_owned();
        let path = if is_tools && item.get("function.parameters").exists() {
            Some("function.parameters")
        } else if item.get("parameters").exists() {
            Some("parameters")
        } else {
            None
        };
        if let Some(path) = path {
            let params = item.get(path);
            if params.kind() == gjson::Kind::Object {
                let normalized = normalize_parameters_schema(params.json());
                if normalized != params.json()
                    && let Ok(next) = set_raw(&raw, path, &normalized)
                {
                    raw = next;
                    changed = true;
                }
            }
        }
        updated.push(raw);
    }
    if !changed {
        return body.to_owned();
    }
    set_raw(body, key, &join_array(&updated)).unwrap_or_else(|_| body.to_owned())
}

fn normalize_parameters_schema(raw: &str) -> String {
    if raw.trim().is_empty() {
        return raw.to_owned();
    }
    let mut params = inline_local_refs(raw);
    for container in ["$defs", "definitions"] {
        if gjson::get(&params, container).exists() {
            params = delete(&params, container);
        }
    }
    if !gjson::get(&params, "type").exists() {
        params = set_str(&params, "type", "object").unwrap_or(params);
    }
    params
}

/// `util.InlineLocalRefs`: expand `#/` JSON pointers against the original schema; sibling
/// keywords override the target and cycles become a "See: <name>" hint.
pub(crate) fn inline_local_refs(raw: &str) -> String {
    if !raw.contains("\"$ref\"") {
        return raw.to_owned();
    }
    let Some(root) = GoValue::parse(raw.trim()) else {
        return raw.to_owned();
    };
    let mut active = std::collections::HashSet::new();
    resolve_refs(&root, &root, &mut active).marshal()
}

fn resolve_refs(root: &GoValue, value: &GoValue, active: &mut std::collections::HashSet<String>) -> GoValue {
    match value {
        GoValue::Array(items) => GoValue::Array(items.iter().map(|i| resolve_refs(root, i, active)).collect()),
        GoValue::Object(node) => {
            if let Some(GoValue::String(reference)) = node.get("$ref")
                && reference.starts_with("#/")
                && let Some(target) = json_pointer(root, reference)
            {
                if active.contains(reference) {
                    return cyclic_fallback(node, target, reference);
                }
                active.insert(reference.clone());
                let resolved = resolve_refs(root, target, active);
                active.remove(reference);
                if let GoValue::Object(mut out) = resolved {
                    for (key, item) in node {
                        if key != "$ref" {
                            out.insert(key.clone(), resolve_refs(root, item, active));
                        }
                    }
                    return GoValue::Object(out);
                }
            }
            GoValue::Object(
                node.iter()
                    .map(|(k, v)| (k.clone(), resolve_refs(root, v, active)))
                    .collect(),
            )
        }
        other => other.clone(),
    }
}

fn json_pointer<'a>(root: &'a GoValue, reference: &str) -> Option<&'a GoValue> {
    let mut current = root;
    for raw in reference.strip_prefix("#/").unwrap_or(reference).split('/') {
        let part = raw.replace("~1", "/").replace("~0", "~");
        current = match current {
            GoValue::Object(map) => map.get(&part)?,
            GoValue::Array(items) => items.get(part.parse::<usize>().ok()?)?,
            _ => return None,
        };
    }
    Some(current)
}

fn cyclic_fallback(node: &std::collections::BTreeMap<String, GoValue>, target: &GoValue, reference: &str) -> GoValue {
    let mut out = std::collections::BTreeMap::new();
    if let GoValue::Object(target) = target {
        for key in ["type", "nullable", "description"] {
            if let Some(value) = target.get(key) {
                out.insert(key.to_owned(), value.clone());
            }
        }
    }
    for (key, value) in node {
        if key != "$ref" {
            out.insert(key.clone(), value.clone());
        }
    }
    let name = reference
        .rfind('/')
        .filter(|i| i + 1 < reference.len())
        .map(|i| reference[i + 1..].replace("~1", "/").replace("~0", "~"))
        .unwrap_or_else(|| reference.to_owned());
    let hint = format!("See: {name}");
    let description = match out.get("description") {
        Some(GoValue::String(existing)) if !existing.is_empty() => {
            if existing == &hint
                || existing.starts_with(&format!("{hint} ("))
                || existing.contains(&format!("({hint})"))
            {
                existing.clone()
            } else {
                format!("{existing} ({hint})")
            }
        }
        _ => hint,
    };
    out.insert("description".into(), GoValue::String(description));
    GoValue::Object(out)
}

/// `normalizeKimiTemperature`: Kimi accepts only 0.6 without thinking and 1.0 with it.
fn normalize_temperature(body: &str) -> String {
    let temperature = gjson::get(body, "temperature");
    if !temperature.exists() {
        return body.to_owned();
    }
    let disabled = gstr(&gjson::get(body, "thinking.type")).eq_ignore_ascii_case("disabled");
    let allowed = if disabled { 0.6 } else { 1.0 };
    if temperature.f64() != allowed {
        delete(body, "temperature")
    } else {
        body.to_owned()
    }
}

fn responses_call_id(item: &gjson::Value<'_>) -> String {
    for key in ["call_id", "tool_call_id", "callId"] {
        let id = item.get(key).str().trim().to_owned();
        if !id.is_empty() {
            return id;
        }
    }
    let id = gstr(&item.get("id")).trim().to_owned();
    if id.starts_with("fco_") { String::new() } else { id }
}

fn is_tool_call(item: &gjson::Value<'_>) -> bool {
    matches!(gstr(&item.get("type")).trim(), "function_call" | "custom_tool_call")
}

fn is_tool_output(item: &gjson::Value<'_>) -> bool {
    matches!(
        gstr(&item.get("type")).trim(),
        "function_call_output" | "custom_tool_call_output"
    )
}

/// `NormalizeKimiResponsesInput`: tool outputs must follow their parallel calls directly;
/// items in between move after the outputs.
fn normalize_responses_input(body: &str) -> String {
    if body.is_empty() || !valid(body) {
        return body.to_owned();
    }
    let input = gjson::get(body, "input");
    if input.kind() != gjson::Kind::Array {
        return body.to_owned();
    }
    let items = input.array();
    if items.is_empty() {
        return body.to_owned();
    }
    let mut reordered = false;
    let mut result: Vec<&str> = Vec::with_capacity(items.len());
    let mut i = 0;
    while i < items.len() {
        if !is_tool_call(&items[i]) {
            result.push(items[i].json());
            i += 1;
            continue;
        }
        let start = i;
        let mut end = i;
        let mut ids: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
        let mut count = 0;
        while end < items.len() && is_tool_call(&items[end]) {
            let id = responses_call_id(&items[end]);
            if !id.is_empty() {
                *ids.entry(id).or_default() += 1;
                count += 1;
            }
            end += 1;
        }
        result.extend(items[start..end].iter().map(|v| v.json()));
        if count == 0 {
            i = end;
            continue;
        }
        let mut needed = ids.clone();
        let mut remaining = count;
        let mut last = None;
        for (j, item) in items.iter().enumerate().skip(end) {
            if remaining == 0 || is_tool_call(item) {
                break;
            }
            if is_tool_output(item) {
                let id = responses_call_id(item);
                if let Some(n) = needed.get_mut(&id).filter(|n| **n > 0) {
                    *n -= 1;
                    remaining -= 1;
                    last = Some(j);
                }
            }
        }
        match last.filter(|_| remaining == 0) {
            Some(last) => {
                let mut outputs = Vec::new();
                let mut between = Vec::new();
                let mut consumed = ids;
                for item in &items[end..=last] {
                    if is_tool_output(item) {
                        let id = responses_call_id(item);
                        if let Some(n) = consumed.get_mut(&id).filter(|n| **n > 0) {
                            *n -= 1;
                            outputs.push(item.json());
                            continue;
                        }
                    }
                    between.push(item.json());
                }
                reordered |= !between.is_empty();
                result.extend(outputs);
                result.extend(between);
                i = last + 1;
            }
            None => i = end,
        }
    }
    if !reordered {
        return body.to_owned();
    }
    set_raw(body, "input", &format!("[{}]", result.join(","))).unwrap_or_else(|_| body.to_owned())
}

/// `restoreClaudeResponseModel`: put the client's model back on a Claude response body or
/// on the `data:` line of an SSE event.
fn restore_response_model(payload: &[u8], model: &str) -> Bytes {
    if model.trim().is_empty() {
        return Bytes::copy_from_slice(payload);
    }
    let Ok(text) = std::str::from_utf8(payload) else {
        return Bytes::copy_from_slice(payload);
    };
    if let Some(updated) = set_model(text, model) {
        return Bytes::from(updated);
    }
    let mut changed = false;
    let lines: Vec<String> = text
        .split('\n')
        .map(|line| {
            let trimmed = line.trim_end_matches('\r');
            if let Some(json) = trimmed.strip_prefix("data:")
                && let Some(updated) = set_model(json.trim(), model)
            {
                changed = true;
                let cr = if line.ends_with('\r') { "\r" } else { "" };
                return format!("data: {updated}{cr}");
            }
            line.to_owned()
        })
        .collect();
    if changed {
        Bytes::from(lines.join("\n"))
    } else {
        Bytes::copy_from_slice(payload)
    }
}

fn set_model(json: &str, model: &str) -> Option<String> {
    if !valid(json) {
        return None;
    }
    let mut out = json.to_owned();
    let mut changed = false;
    for path in ["model", "message.model"] {
        if gjson::get(&out, path).exists()
            && let Ok(next) = set_str(&out, path, model)
        {
            out = next;
            changed = true;
        }
    }
    changed.then_some(out)
}

#[cfg(test)]
#[path = "kimi_tests.rs"]
mod tests;
