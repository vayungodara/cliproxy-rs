//! Kimi executor (internal/runtime/executor/kimi_executor.go).
//!
//! Three wire paths, chosen by the client format:
//! - Claude Messages: delegated to the Claude executor against Kimi's Anthropic-compatible
//!   base, with model normalization, response-model restoration and native reasoning
//!   replay (kimi_replay).
//! - OpenAI Responses: native `/v1/responses`, body kept in Responses shape.
//! - Everything else: translated to OpenAI Chat Completions at `/v1/chat/completions`.
//!
//! JSON reads and edits use cpa_common::json, thinking cpa_common::thinking, request
//! translation cpa_translate, and clients and Go net/http wire behaviour the shared proxy
//! module. Stages whose shared module has not landed go through adapters named after their
//! owners: custom headers (kimi_http), payload rules and Codex-client rewrites (below).
//! Translator apply_patch failures end streams with Go's 502. The native Responses path
//! runs Go's apply_patch Responses bridge (cpa_translate::apply_patch_responses): a
//! client's custom `apply_patch` tool goes upstream as a JSON function and comes back as
//! the custom tool call.

use std::collections::VecDeque;
use std::sync::Arc;

use bytes::Bytes;
use cpa_common::gostr::GoStr;
use cpa_common::json::{self as gj, GoValue, Kind, Res};
use cpa_common::thinking::{ModelCaps, RequestThinking, apply_request_thinking, parse_suffix};
use cpa_core::config::Config;
use cpa_core::credential::{Credential, MetadataPatch};
use cpa_core::exec::{ExecError, ExecRequest, ExecResponse, ExecStream, FailureScope, Operation, ResponseBody};
use cpa_core::format::Format;
use cpa_translate::{RequestCtx, ResponseCtx, StreamTranslator, apply_patch_responses};
use futures_util::StreamExt;

use crate::claude::{ClaudeExecutor, Delegation};
use crate::kimi_auth::{self, DeviceFlow};
use crate::kimi_http::{
    BUILD_VERSION, credential_headers, go_arch, go_os, hostname, payload_rules, refresh_due, rfc3339_local_now,
    status_error,
};
use crate::kimi_http::{DeferredUsage, UsageRule, apply_patch_requested, defer_usage, report_model};
use crate::kimi_replay::{self, ReplayCache};
use crate::meta_codex::go_trim_space;
use crate::openai_compat_payload::ensure_responses_usage_details;
use crate::proxy::{GoClients, GoHeaders, Proxy, Upstream, default_client, lines, read_all, send};

const REASONING_UNAVAILABLE: &str = "[reasoning unavailable]";

/// Provider strings served by this executor.
pub const PROVIDERS: [&str; 4] = ["kimi", "kimi-ai", "kimi.ai", "kimi.com"];

/// Claude-format traffic goes through the shared [`ClaudeExecutor`] passed to
/// [`KimiExecutor::execute`]; Go embeds one in the Kimi executor for the same purpose.
pub struct KimiExecutor {
    clients: GoClients,
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
            clients: GoClients::with_default(client),
            oauth_host: None,
            replay: Arc::default(),
        }
    }

    /// Sends token refreshes to a local mock instead of auth.kimi.com / auth.kimi.ai.
    pub fn with_oauth_host(mut self, host: &str) -> Self {
        self.oauth_host = Some(host.to_owned());
        self
    }

    pub fn needs_prepare(&self, credential: &Credential, cfg: &Config) -> bool {
        self.needs_prepare_at(credential, cfg, chrono::Utc::now())
    }

    pub fn needs_prepare_at(&self, credential: &Credential, _cfg: &Config, now: chrono::DateTime<chrono::Utc>) -> bool {
        // Go's refresh loop never schedules API-key-kind credentials.
        !cpa_core::registry::dynamic::is_api_key(credential)
            && refresh_token(credential).is_some()
            && refresh_due(
                credential,
                chrono::Duration::from_std(kimi_auth::REFRESH_LEAD).ok(),
                now,
            )
    }

    /// Refresh grant; Kimi has no other preparation.
    pub async fn prepare(&self, credential: &Credential, cfg: &Config) -> Result<MetadataPatch, ExecError> {
        let Some(refresh) = refresh_token(credential) else {
            return Ok(MetadataPatch::default());
        };
        let domain = kimi_auth::resolve_domain(credential);
        let device_id = credential.str("device_id").unwrap_or_default();
        let mut flow = DeviceFlow::new(self.clients.get(&Proxy::effective(credential, cfg)), domain, device_id);
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
            let mut req = req;
            // Go CountTokens: Responses requests declare apply_patch as a function first.
            if req.operation == Operation::CountTokens && req.source_format == Format::OpenAIResponse {
                req.body = Bytes::from(
                    apply_patch_responses::normalize_executor_request(&req.body, None).map_err(internal_error)?,
                );
            }
            return self.execute_claude(claude, credential, req, cfg).await;
        }
        let client = self.clients.get(&Proxy::effective(credential, cfg));
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
        // NewKimiExecutor's embedded ClaudeExecutor: normalized upstream model, count_tokens
        // always upstream (KimiExecutor.CountTokens -> countTokensUpstream), and request
        // logs naming kimi (requestLogProvider).
        let delegation = Delegation {
            upstream_model: Some(normalize_upstream_model),
            count_upstream: true,
            request_log_provider: Some("kimi"),
        };
        if req.operation == Operation::CountTokens {
            return claude.execute_delegated(&delegated, req, cfg, delegation).await;
        }
        let scope = kimi_replay::prepare(&self.replay, &mut req).await;
        let streaming = req.stream;
        let mut response = match claude.execute_delegated(&delegated, req, cfg, delegation).await {
            Ok(response) => response,
            Err(error) => {
                if scope.applied && kimi_replay::clears_after(&error) {
                    scope.clear();
                }
                scope.writes.settle().await;
                return Err(error);
            }
        };
        response.body = match response.body {
            ResponseBody::Buffered(body) => {
                let body = restore_response_model(&body, &client_model);
                if !streaming {
                    scope.store_response(&body);
                }
                scope.writes.settle().await;
                ResponseBody::Buffered(body)
            }
            ResponseBody::Stream(stream) => {
                let restored = stream
                    .map(move |event| event.map(|e| restore_response_model(&e, &client_model)))
                    .boxed();
                let writes = scope.writes.clone();
                ResponseBody::Stream(writes.gate(crate::replay::wrap_stream(restored, scope)))
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
    for (name, value) in credential_headers(credential, req, original(req)) {
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

/// `opts.OriginalRequest`, falling back to the request payload when empty.
fn original(req: &ExecRequest) -> &Bytes {
    if req.original_body.is_empty() {
        &req.body
    } else {
        &req.original_body
    }
}

/// `helps.ApplyRequestThinking` for Kimi. `to` is Go's target format name: `kimi` for
/// Chat Completions (no translator targets it) and `codex` for native Responses.
fn thinking(body: &[u8], req: &ExecRequest, to: &str) -> Result<Vec<u8>, ExecError> {
    let has_request_transformer =
        Format::parse(to).is_some_and(|t| cpa_translate::pair(req.source_format, t).is_some());
    // Go `cliproxyauth.ResolvedModelInfo`: capabilities bound to this attempt.
    let caps = req.resolved_model.as_ref().map(|r| ModelCaps::from(&r.info));
    apply_request_thinking(&RequestThinking {
        body,
        payload: &req.body,
        original: original(req),
        model: &req.model,
        from: req.source_format.as_str(),
        to,
        provider: "kimi",
        resolved: caps.as_ref().map(Some),
        has_request_transformer,
        updates_changed: false,
    })
    .map_err(|e| ExecError::local(e.status(), FailureScope::Request, e.message))
}

/// Go's bufio.Scanner limits: 1 MiB for Chat Completions lines, 50 MiB for Responses.
const CHAT_LINE_LIMIT: usize = 1_048_576;
const RESPONSES_LINE_LIMIT: usize = 52_428_800;

/// One upstream send (kimi_executor.go's four `RecordAPIRequest` sites): the capture of
/// the request, its response metadata and a rejected response's body, then the send.
async fn post(
    client: &wreq::Client,
    credential: &Credential,
    req: &ExecRequest,
    url: &str,
    headers: GoHeaders,
    body: Vec<u8>,
) -> Result<Upstream, ExecError> {
    use crate::kimi_http::{account_info, capture_chunk, capture_error, capture_metadata, capture_request};
    let capture = req.capture();
    let (auth_type, auth_value) = account_info(credential);
    capture_request(
        capture,
        credential,
        url,
        &headers,
        &body,
        "kimi",
        (auth_type, &auth_value),
    );
    // Go's TrackHTTPClient: the TTFT runs from the request to the first body byte.
    req.usage.round_trip_started();
    let mut upstream = send(client, url, headers, body, None)
        .await
        .inspect_err(|e| capture_error(capture, e))?;
    capture_metadata(capture, upstream.status, &upstream.headers);
    upstream.body = crate::kimi_http::track_first_byte(upstream.body, &req.usage, false);
    if !(200..300).contains(&upstream.status) {
        let error = status_error(upstream).await;
        // ponytail: the error body is the MAX_ERROR_BODY prefix (proxy.rs ceiling); Go
        // logs the whole body.
        capture_chunk(capture, &error.body);
        return Err(error);
    }
    Ok(upstream)
}

/// Go's non-stream `io.ReadAll` with its capture: a read error, else the whole body.
async fn read_body(
    body: futures_util::stream::BoxStream<'static, Result<Bytes, ExecError>>,
    capture: &cpa_core::exec::CaptureSink,
) -> Result<Bytes, ExecError> {
    let data = read_all(body, usize::MAX, false)
        .await
        .inspect_err(|e| crate::kimi_http::capture_error(capture, e))?;
    crate::kimi_http::capture_chunk(capture, &data);
    Ok(data)
}

/// Chat Completions path (Execute / ExecuteStream for non-Claude, non-Responses clients).
async fn execute_chat(
    client: &wreq::Client,
    credential: &Credential,
    req: ExecRequest,
    cfg: &Config,
) -> Result<ExecResponse, ExecError> {
    let base_model = parse_suffix(&req.model).model_name;
    if cpa_translate::pair(req.source_format, Format::OpenAI).is_none() {
        return Err(not_registered("Kimi request"));
    }
    let Some(response_pair) = cpa_translate::pair(req.response_format, Format::OpenAI) else {
        return Err(not_registered("Kimi response"));
    };
    // Go: helps.TranslateRequestWithCodexMultiAgentV2 (no target executor, not compat).
    let codex_client = crate::codex_client::Client::new(&req.headers, cfg, "", false);
    let translate = |body: &[u8], stream: bool| {
        crate::codex_client::translate_request(
            req.source_format,
            Format::OpenAI,
            &RequestCtx {
                model: &base_model,
                stream,
            },
            body,
            &codex_client,
        )
        .map_err(|e| request_error(e.to_string()))
    };
    let translated = translate(&req.body, req.stream)?;
    // Go translates the original request too (stream false) for payload-rule defaults.
    let original_translated = translate(original(&req), false)?;
    let upstream_model = normalize_upstream_model(&base_model);
    let mut body = gj::try_set_str(&translated, "model", &upstream_model)
        .map_err(|e| internal_error(format!("kimi executor: failed to set model in payload: {e}")))?;
    body = thinking(&body, &req, "kimi")?;
    if req.stream {
        body = gj::try_set_raw(&body, "stream_options.include_usage", "true")
            .map_err(|e| internal_error(format!("kimi executor: failed to set stream_options in payload: {e}")))?;
    }
    body = payload_rules(cfg, &req, &base_model, "openai", body, &original_translated);
    body = normalize_tool_message_links(body)?;
    body = normalize_tools(body);
    body = normalize_temperature(body);
    req.usage.upstream_model(&upstream_model);
    req.usage.request_for("kimi", &body);
    if req.stream {
        // Go's stream publishes only through its usage buffer (no EnsurePublished).
        req.usage.usage_required();
    }
    let upstream = post(
        client,
        credential,
        &req,
        &chat_url(credential),
        headers(credential, &req, req.stream),
        body.clone(),
    )
    .await?;
    let translated = Bytes::from(body);
    let ctx = ResponseCtx {
        model: &req.model,
        original_request: original(&req),
        translated_request: &translated,
    };
    let body = if req.stream {
        let (tapped, usage) = defer_usage(
            crate::kimi_http::capture_lines(lines(upstream.body, CHAT_LINE_LIMIT), req.capture()),
            &req.usage,
            UsageRule::OpenAIStream,
        );
        ResponseBody::Stream(translate_lines(
            tapped,
            (response_pair.stream)(&ctx),
            StreamEnd::Chat,
            usage,
        ))
    } else {
        let data = read_body(upstream.body, req.capture()).await?;
        // Go observes the response model now and publishes the usage once translated.
        report_model(&req.usage, Format::OpenAI, &data);
        // A translator error or empty output is Go's apply_patch 502.
        let out = (response_pair.non_stream)(&ctx, &data)
            .ok()
            .filter(|out| !out.is_empty())
            .ok_or_else(apply_patch_error)?;
        req.usage.response_body(Format::OpenAI, &data);
        ResponseBody::Buffered(Bytes::from(if req.response_format == Format::OpenAIResponse {
            ensure_responses_usage_details(&out)
        } else {
            out
        }))
    };
    Ok(ExecResponse {
        status: upstream.status,
        headers: upstream.headers,
        body,
    })
}

/// How the Go loop ends a translated stream.
#[derive(Clone, Copy, PartialEq, Eq)]
enum StreamEnd {
    /// Chat Completions (executeStream): EndApplyPatchStream, then a synthetic `[DONE]`.
    Chat,
    /// Native Responses translated for another client (executeResponsesStream): neither.
    Responses,
}

/// The 502 Go returns when a translator rejects upstream apply_patch input.
fn apply_patch_error() -> ExecError {
    ExecError::local(502, FailureScope::Request, cpa_translate::APPLY_PATCH_UPSTREAM_ERROR)
}

/// Go's per-line stream loop: each non-empty scanned line goes through the translator.
/// A translator that rejects apply_patch input ends the stream after that line's frames
/// with a 502 (StopApplyPatchStream). At the end Chat streams finalize tool input
/// (EndApplyPatchStream) and translate a synthetic `[DONE]`, even after a scan error; the
/// scan error follows. Before any terminal error the Responses route flushes the frame it
/// is still joining.
fn translate_lines(
    upstream: ExecStream,
    translator: Box<dyn StreamTranslator>,
    end: StreamEnd,
    usage: DeferredUsage,
) -> ExecStream {
    struct State {
        upstream: ExecStream,
        translator: Box<dyn StreamTranslator>,
        ready: VecDeque<Result<Bytes, ExecError>>,
        done: bool,
        end: StreamEnd,
        usage: DeferredUsage,
        failed: bool,
    }
    impl State {
        fn fail(&mut self, error: ExecError) {
            self.done = true;
            self.failed = true;
            let flushed = self.translator.flush_frames();
            self.ready.extend(flushed.into_iter().map(Ok));
            self.ready.push_back(Err(error));
        }

        /// One translated line; false when the stream has ended. Bridge frames end in a
        /// blank line; Go hands the translator the whole chunk, which it trims.
        fn translate(&mut self, line: &[u8]) -> bool {
            let line = line.strip_suffix(b"\n\n").unwrap_or(line);
            match self.translator.event(line) {
                Ok(events) => self.ready.extend(events.into_iter().map(Ok)),
                Err(error) => {
                    self.fail(translate_error(error));
                    return false;
                }
            }
            true
        }

        fn finish(&mut self, end: Option<Result<Bytes, ExecError>>) {
            self.done = true;
            if self.end == StreamEnd::Chat {
                let finalized = self.translator.finalize_tool_input();
                self.ready.extend(finalized.into_iter().map(Ok));
                if self.translator.tool_input_failed() {
                    return self.fail(apply_patch_error());
                }
                if !self.translate(b"[DONE]") {
                    return;
                }
            }
            match self.translator.finish() {
                Ok(events) => self.ready.extend(events.into_iter().map(Ok)),
                Err(error) => return self.fail(translate_error(error)),
            }
            if let Some(Err(error)) = end {
                return self.fail(error);
            }
            if !self.failed {
                self.usage.commit();
            }
        }
    }
    futures_util::stream::unfold(
        State {
            upstream,
            translator,
            ready: VecDeque::new(),
            done: false,
            end,
            usage,
            failed: false,
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
                    Some(Ok(line)) => {
                        if st.translate(&line) && st.translator.tool_input_failed() {
                            st.fail(apply_patch_error());
                        }
                    }
                    end => st.finish(end),
                }
            }
        },
    )
    .boxed()
}

fn translate_error(error: cpa_translate::Error) -> ExecError {
    ExecError::local(502, FailureScope::Request, error.to_string())
}

/// Native Responses streaming: Go writes every scanned line plus `\n` as one chunk and
/// the Responses route joins chunks into frames (cpa_translate's ResponsesFramer),
/// flushing what is pending at the end and before a terminal error.
fn responses_frames(lines: ExecStream, usage: DeferredUsage) -> ExecStream {
    struct State {
        lines: ExecStream,
        joiner: cpa_translate::stream::ResponsesFramer,
        ready: VecDeque<Result<Bytes, ExecError>>,
        done: bool,
        usage: DeferredUsage,
    }
    futures_util::stream::unfold(
        State {
            lines,
            joiner: Default::default(),
            ready: VecDeque::new(),
            done: false,
            usage,
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
                        match end {
                            Some(Err(error)) => st.ready.push_back(Err(error)),
                            _ => st.usage.commit(),
                        }
                    }
                }
            }
        },
    )
    .boxed()
}

/// Go's native Responses loop around the apply_patch bridge: every scanned line goes
/// through `State::stream`; at EOF `finish_stream` runs before a scan error is reported,
/// and a bridge failure ends the stream with the apply_patch 502.
fn bridge_lines(lines: ExecStream, bridge: apply_patch_responses::State) -> ExecStream {
    futures_util::stream::unfold(
        (lines, bridge, VecDeque::<Result<Bytes, ExecError>>::new(), false),
        |(mut lines, mut bridge, mut ready, mut done)| async move {
            loop {
                if let Some(item) = ready.pop_front() {
                    return Some((item, (lines, bridge, ready, done)));
                }
                if done {
                    return None;
                }
                match lines.next().await {
                    Some(Ok(line)) => {
                        let (out, error) = bridge.stream(&line);
                        ready.extend(out.into_iter().map(|l| Ok(Bytes::from(l))));
                        if error.is_some() {
                            done = true;
                            ready.push_back(Err(apply_patch_error()));
                        }
                    }
                    end => {
                        done = true;
                        let (out, error) = bridge.finish_stream();
                        ready.extend(out.into_iter().map(|l| Ok(Bytes::from(l))));
                        if error.is_some() {
                            ready.push_back(Err(apply_patch_error()));
                        } else if let Some(Err(error)) = end {
                            ready.push_back(Err(error));
                        }
                    }
                }
            }
        },
    )
    .boxed()
}

/// The usage Go's native Responses Execute publishes: Codex usage (`response.usage`)
/// with tokens, else the top-level OpenAI usage with tokens, else none. Returned as the
/// body the usage sink parses (top-level `usage`, Go's tier).
fn native_usage_body(data: &[u8]) -> Option<Vec<u8>> {
    use crate::kimi_http::usage_has_tokens;
    if usage_has_tokens(&gj::get(data, "response.usage")) {
        let mut inner = gj::get(data, "response").raw().to_vec();
        let tier = crate::kimi_http::response_tier(data);
        if tier.is_empty() {
            gj::delete(&mut inner, "service_tier");
        } else {
            gj::set_str(&mut inner, "service_tier", tier);
        }
        return Some(inner);
    }
    usage_has_tokens(&gj::get(data, "usage")).then(|| data.to_vec())
}

/// `SetBoolIfDifferent`.
fn set_bool_if_different(mut body: Vec<u8>, path: &str, value: bool) -> Vec<u8> {
    let kind = gj::get(&body, path).kind;
    if (value && kind == Kind::True) || (!value && kind == Kind::False) {
        return body;
    }
    gj::set_bool(&mut body, path, value);
    body
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
    let base_model = parse_suffix(&req.model).model_name;
    let upstream_model = normalize_upstream_model(&base_model);
    let mut body = gj::try_set_str(&req.body, "model", &upstream_model)
        .map_err(|e| internal_error(format!("kimi executor: failed to set model in payload: {e}")))?;
    body = set_bool_if_different(body, "stream", req.stream);
    body = thinking(&body, &req, "codex")?;
    // Go passes req.Payload as the original here, not OriginalRequest.
    body = payload_rules(cfg, &req, &base_model, "openai-response", body, &req.body);
    body = apply_patch_responses::normalize_executor_request(&body, None).map_err(internal_error)?;
    body = normalize_responses_input(body);
    body = normalize_tools(body);
    body = normalize_temperature(body);
    req.usage.upstream_model(&upstream_model);
    req.usage.request_for("kimi", &body);
    // Go publishes native Responses usage only when it has tokens (stream buffer, or the
    // non-stream Codex/OpenAI usage check); there is no EnsurePublished.
    req.usage.usage_required();
    let upstream = post(
        client,
        credential,
        &req,
        &responses_url(credential),
        headers(credential, &req, req.stream),
        body.clone(),
    )
    .await?;
    let translated = Bytes::from(body);
    let ctx = ResponseCtx {
        model: &req.model,
        original_request: original(&req),
        translated_request: &translated,
    };
    let original_request = original(&req);
    let mut bridge = apply_patch_responses::State::new(req.source_format, original_request, original_request);
    let out = if req.stream {
        // Usage comes from the upstream lines; the bridge sits between them and the client.
        let (tapped, usage) = defer_usage(
            crate::kimi_http::capture_lines(lines(upstream.body, RESPONSES_LINE_LIMIT), req.capture()),
            &req.usage,
            UsageRule::KimiResponses,
        );
        let bridged = if bridge.active() {
            bridge_lines(tapped, bridge)
        } else {
            tapped
        };
        match response_pair {
            Some(pair) => ResponseBody::Stream(translate_lines(
                bridged,
                (pair.stream)(&ctx),
                StreamEnd::Responses,
                usage,
            )),
            // Native Responses clients get every line back, joined into frames.
            None => ResponseBody::Stream(responses_frames(bridged, usage)),
        }
    } else {
        let data = read_body(upstream.body, req.capture()).await?;
        report_model(&req.usage, Format::Codex, &data);
        let out = match bridge.bridge.transform_non_stream(&data) {
            Ok(out) if !(out.is_empty() && apply_patch_requested(original_request)) => out,
            _ => return Err(apply_patch_error()),
        };
        let out = match response_pair {
            Some(pair) => (pair.non_stream)(&ctx, &out)
                .ok()
                .filter(|out| !out.is_empty())
                .ok_or_else(apply_patch_error)?,
            None => out,
        };
        if let Some(usage) = native_usage_body(&data) {
            req.usage.response_body(Format::Codex, &usage);
        }
        ResponseBody::Buffered(Bytes::from(out))
    };
    Ok(ExecResponse {
        status: upstream.status,
        headers: upstream.headers,
        body: out,
    })
}

/// `stripKimiPrefix`.
fn strip_kimi_prefix(model: &str) -> &str {
    let model = model.trim();
    if model.go_lower().starts_with("kimi-") {
        &model[5..]
    } else {
        model
    }
}

/// `normalizeKimiUpstreamModel`: strip `kimi-` and `[1m]`, map K2.7/K2.8 Code aliases,
/// keep a thinking suffix.
pub(crate) fn normalize_upstream_model(model: &str) -> String {
    let parsed = parse_suffix(model.trim());
    let mut base = parsed.model_name.trim().go_lower();
    if let Some(stripped) = base.strip_suffix("[1m]") {
        base = stripped.to_owned();
    }
    let normalized = match base.as_str() {
        "kimi-k2.8" | "k2.8" | "kimi-k2.8-code" | "k2.8-code" | "kimi-k2.8-preview" | "k2.8-preview"
        | "kimi-k2.7-code" | "k2.7-code" | "kimi-for-coding" | "for-coding" => "kimi-for-coding".to_owned(),
        "kimi-k2.7-code-highspeed" | "k2.7-code-highspeed" | "kimi-for-coding-highspeed" | "for-coding-highspeed" => {
            "kimi-for-coding-highspeed".to_owned()
        }
        _ => strip_kimi_prefix(&base).to_owned(),
    };
    if parsed.has_suffix {
        format!("{normalized}({})", parsed.raw_suffix)
    } else {
        normalized
    }
}

fn usable_reasoning(reasoning: &str) -> bool {
    let trimmed = reasoning.trim();
    !trimmed.is_empty() && trimmed != REASONING_UNAVAILABLE
}

/// `strings.TrimSpace(r.String())`, as bytes.
fn trimmed(r: &Res<'_>) -> Vec<u8> {
    go_trim_space(&r.bytes()).to_vec()
}

fn is_empty_object(r: &Res<'_>) -> bool {
    go_trim_space(r.raw()) == b"{}"
}

fn content_part_empty(part: &Res<'_>) -> bool {
    match part.kind {
        Kind::Null => true,
        Kind::String => trimmed(part).is_empty(),
        Kind::Json if part.is_object() => {
            let text = part.get("text");
            if text.exists() {
                return trimmed(&text).is_empty();
            }
            trimmed(&part.get("type")) == b"text" || is_empty_object(part)
        }
        _ => false,
    }
}

fn should_drop_assistant(msg: &Res<'_>) -> bool {
    if trimmed(&msg.get("role")) != b"assistant" {
        return false;
    }
    let tool_calls = msg.get("tool_calls");
    let has_tool_calls = tool_calls.is_array() && !tool_calls.array().is_empty();
    let call = msg.get("function_call");
    let has_function_call = call.exists() && call.kind != Kind::Null && !(call.is_object() && is_empty_object(&call));
    let has_reasoning = {
        let r = msg.get("reasoning_content");
        r.exists() && !trimmed(&r).is_empty()
    };
    if has_tool_calls || has_function_call || has_reasoning {
        return false;
    }
    let content = msg.get("content");
    match content.kind {
        _ if !content.exists() => true,
        Kind::Null => true,
        Kind::String => trimmed(&content).is_empty(),
        Kind::Json if content.is_array() => content.array().iter().all(content_part_empty),
        _ => false,
    }
}

fn fallback_reasoning(msg: &Res<'_>, latest: Option<&[u8]>) -> Vec<u8> {
    if let Some(latest) = latest.filter(|l| usable_reasoning(&String::from_utf8_lossy(l))) {
        return latest.to_vec();
    }
    let content = msg.get("content");
    if content.kind == Kind::String && !trimmed(&content).is_empty() {
        return trimmed(&content);
    }
    if content.is_array() {
        let parts: Vec<Vec<u8>> = content
            .array()
            .iter()
            .map(|item| trimmed(&item.get("text")))
            .filter(|t| !t.is_empty())
            .collect();
        if !parts.is_empty() {
            return parts.join(&b'\n');
        }
    }
    REASONING_UNAVAILABLE.as_bytes().to_vec()
}

/// `normalizeKimiToolMessageLinks`: drop empty assistant turns, repair tool_call_id links
/// and give tool-calling assistant turns reasoning_content.
fn normalize_tool_message_links(body: Vec<u8>) -> Result<Vec<u8>, ExecError> {
    if body.is_empty() || !gj::valid(&body) {
        return Ok(body);
    }
    let messages = gj::get(&body, "messages");
    if !messages.is_array() {
        return Ok(body);
    }
    let msgs = messages.array();
    let mut dropped = vec![false; msgs.len()];
    let mut patches: Vec<(usize, &str, Vec<u8>, &str)> = Vec::new();
    let mut pending: Vec<Vec<u8>> = Vec::new();
    let mut latest: Option<Vec<u8>> = None;
    for (index, msg) in msgs.iter().enumerate() {
        if should_drop_assistant(msg) {
            dropped[index] = true;
            continue;
        }
        match trimmed(&msg.get("role")).as_slice() {
            b"assistant" => {
                let reasoning = msg.get("reasoning_content");
                let usable = reasoning.exists() && usable_reasoning(&reasoning.str());
                if usable {
                    latest = Some(reasoning.bytes().into_owned());
                }
                let calls = msg.get("tool_calls");
                if calls.is_array() && !calls.array().is_empty() {
                    if !usable {
                        patches.push((
                            index,
                            "reasoning_content",
                            fallback_reasoning(msg, latest.as_deref()),
                            "failed to set assistant reasoning_content",
                        ));
                    }
                    for call in calls.array() {
                        let id = trimmed(&call.get("id"));
                        if !id.is_empty() {
                            pending.push(id);
                        }
                    }
                }
            }
            b"tool" => {
                let mut id = trimmed(&msg.get("tool_call_id"));
                if id.is_empty() {
                    id = trimmed(&msg.get("call_id"));
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
        return Ok(body);
    }
    if !any_dropped && patches.len() == 1 {
        let (index, path, value, context) = &patches[0];
        return gj::try_set_str(&body, &format!("messages.{index}.{path}"), value)
            .map_err(|e| internal_error(format!("kimi executor: {context}: {e}")));
    }
    let mut items = Vec::with_capacity(msgs.len());
    let mut next = patches.iter().peekable();
    for (index, msg) in msgs.iter().enumerate() {
        if dropped[index] {
            continue;
        }
        let mut raw = msg.raw().to_vec();
        while let Some((_, path, value, context)) = next.next_if(|p| p.0 == index) {
            raw = gj::try_set_str(&raw, path, value)
                .map_err(|e| internal_error(format!("kimi executor: {context}: {e}")))?;
        }
        items.push(raw);
    }
    gj::try_set_raw(&body, "messages", gj::join(&items)).map_err(|e| {
        let context = if any_dropped {
            "failed to drop empty assistant messages"
        } else {
            patches[0].3
        };
        internal_error(format!("kimi executor: {context}: {e}"))
    })
}

/// `normalizeKimiTools`: inline local `$ref`s and default the root schema type.
fn normalize_tools(body: Vec<u8>) -> Vec<u8> {
    if body.is_empty() {
        return body;
    }
    let body = normalize_tool_list(body, "tools", true);
    normalize_tool_list(body, "functions", false)
}

fn normalize_tool_list(body: Vec<u8>, key: &str, is_tools: bool) -> Vec<u8> {
    let items = gj::get(&body, key);
    if !items.is_array() {
        return body;
    }
    let items = items.array();
    if items.is_empty() {
        return body;
    }
    let mut changed = false;
    let mut updated = Vec::with_capacity(items.len());
    for item in &items {
        let mut raw = item.raw().to_vec();
        let path = if is_tools && item.get("function.parameters").exists() {
            Some("function.parameters")
        } else if item.get("parameters").exists() {
            Some("parameters")
        } else {
            None
        };
        if let Some(path) = path {
            let params = item.get(path);
            if params.is_object() {
                let normalized = normalize_parameters_schema(params.raw());
                if normalized != params.raw()
                    && let Ok(next) = gj::try_set_raw(&raw, path, &normalized)
                {
                    raw = next;
                    changed = true;
                }
            }
        }
        updated.push(raw);
    }
    if !changed {
        return body;
    }
    gj::try_set_raw(&body, key, gj::join(&updated)).unwrap_or(body)
}

fn normalize_parameters_schema(raw: &[u8]) -> Vec<u8> {
    if go_trim_space(raw).is_empty() {
        return raw.to_vec();
    }
    let mut params = inline_local_refs(raw);
    for container in ["$defs", "definitions"] {
        if gj::get(&params, container).exists() {
            gj::delete(&mut params, container);
        }
    }
    if !gj::get(&params, "type").exists() {
        gj::set_str(&mut params, "type", "object");
    }
    params
}

/// `util.InlineLocalRefs`: expand `#/` JSON pointers against the original schema; sibling
/// keywords override the target and cycles become a "See: <name>" hint.
pub(crate) fn inline_local_refs(raw: &[u8]) -> Vec<u8> {
    if !raw.windows(6).any(|w| w == b"\"$ref\"") {
        return raw.to_vec();
    }
    let Some(root) = GoValue::parse(go_trim_space(raw)) else {
        return raw.to_vec();
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
fn normalize_temperature(mut body: Vec<u8>) -> Vec<u8> {
    let temperature = gj::get(&body, "temperature");
    if !temperature.exists() {
        return body;
    }
    let disabled = gj::get(&body, "thinking.type").str().go_eq_fold("disabled");
    let allowed = if disabled { 0.6 } else { 1.0 };
    if temperature.float() != allowed {
        gj::delete(&mut body, "temperature");
    }
    body
}

fn responses_call_id(item: &Res<'_>) -> Vec<u8> {
    for key in ["call_id", "tool_call_id", "callId"] {
        let id = trimmed(&item.get(key));
        if !id.is_empty() {
            return id;
        }
    }
    let id = trimmed(&item.get("id"));
    if id.starts_with(b"fco_") { Vec::new() } else { id }
}

fn is_tool_call(item: &Res<'_>) -> bool {
    matches!(
        trimmed(&item.get("type")).as_slice(),
        b"function_call" | b"custom_tool_call"
    )
}

fn is_tool_output(item: &Res<'_>) -> bool {
    matches!(
        trimmed(&item.get("type")).as_slice(),
        b"function_call_output" | b"custom_tool_call_output"
    )
}

/// `NormalizeKimiResponsesInput`: tool outputs must follow their parallel calls directly;
/// items in between move after the outputs.
fn normalize_responses_input(body: Vec<u8>) -> Vec<u8> {
    if body.is_empty() || !gj::valid(&body) {
        return body;
    }
    let input = gj::get(&body, "input");
    if !input.is_array() {
        return body;
    }
    let items = input.array();
    if items.is_empty() {
        return body;
    }
    let mut reordered = false;
    let mut result: Vec<&[u8]> = Vec::with_capacity(items.len());
    let mut i = 0;
    while i < items.len() {
        if !is_tool_call(&items[i]) {
            result.push(items[i].raw());
            i += 1;
            continue;
        }
        let start = i;
        let mut end = i;
        let mut ids: std::collections::HashMap<Vec<u8>, usize> = std::collections::HashMap::new();
        let mut count = 0;
        while end < items.len() && is_tool_call(&items[end]) {
            let id = responses_call_id(&items[end]);
            if !id.is_empty() {
                *ids.entry(id).or_default() += 1;
                count += 1;
            }
            end += 1;
        }
        result.extend(items[start..end].iter().map(|v| v.raw()));
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
                            outputs.push(item.raw());
                            continue;
                        }
                    }
                    between.push(item.raw());
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
        return body;
    }
    let joined = gj::join(&result);
    gj::try_set_raw(&body, "input", joined).unwrap_or(body)
}

/// `restoreClaudeResponseModel`: put the client's model back on a Claude response body or
/// on the `data:` line of an SSE event.
fn restore_response_model(payload: &[u8], model: &str) -> Bytes {
    if model.trim().is_empty() {
        return Bytes::copy_from_slice(payload);
    }
    if let Some(updated) = set_model(payload, model) {
        return Bytes::from(updated);
    }
    let mut changed = false;
    let lines: Vec<Vec<u8>> = payload
        .split(|b| *b == b'\n')
        .map(|line| {
            let (content, cr) = match line.strip_suffix(b"\r") {
                Some(content) => (content, &b"\r"[..]),
                None => (line, &b""[..]),
            };
            if let Some(json) = content.strip_prefix(b"data:")
                && let Some(updated) = set_model(go_trim_space(json), model)
            {
                changed = true;
                return [&b"data: "[..], &updated, cr].concat();
            }
            line.to_vec()
        })
        .collect();
    if changed {
        Bytes::from(lines.join(&b'\n'))
    } else {
        Bytes::copy_from_slice(payload)
    }
}

fn set_model(json: &[u8], model: &str) -> Option<Vec<u8>> {
    if !gj::valid(json) {
        return None;
    }
    let mut out = json.to_vec();
    let mut changed = false;
    for path in ["model", "message.model"] {
        if gj::get(&out, path).exists() && gj::set_str(&mut out, path, model) {
            changed = true;
        }
    }
    changed.then_some(out)
}

#[cfg(test)]
#[path = "kimi_tests.rs"]
mod tests;
