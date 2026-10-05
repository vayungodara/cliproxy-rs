//! Devin executor (internal/runtime/executor/devin_executor.go).
//!
//! Every client format is translated to Gemini Interactions, parsed into Devin's chat
//! model (system prompt, history prompts, tools), and sent as one Connect-RPC
//! `GetChatMessage` protobuf to the Codeium server. The streamed Connect frames
//! (text, thinking, signatures, tool-call deltas, usage, an EOS trailer) become
//! Interactions events, which the registered Interactions translator turns into the
//! client's format; non-streaming clients get one Interactions response built from
//! all frames.
//!
//! Credentials are permanent session tokens; "refresh" only re-reads seat, plan and
//! quota from `GetUserStatus` (and only when `refresh_interval` asks for it, as Go's
//! nil refresh lead implies).
//!
//! Request logging records Go's readable log bodies instead of the protobuf
//! (`devin_wire::request_log_body`, `response_log_body` and the stream summary).

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bytes::Bytes;
use cpa_common::json::{self as gj};
use cpa_common::thinking::parse_suffix;
use cpa_core::config::Config;
use cpa_core::credential::{Credential, MetadataPatch};
use cpa_core::exec::{ExecError, ExecRequest, ExecResponse, ExecStream, FailureScope, Operation, ResponseBody};
use cpa_core::format::Format;
use cpa_translate::{ResponseCtx, StreamTranslator};
use futures_util::StreamExt;
use serde_json::Value;

use crate::codex_quota::Snapshot;
use crate::devin_auth::DevinAuth;
use crate::devin_models::resolve_chat_model_uid;
use crate::devin_request::parse_interactions;
use crate::devin_wire::{
    CHAT_PATH, ChatRequest, DEFAULT_BASE_URL, FLAG_END_STREAM, FrameError, FrameReader, RequestLog, ResponseLog,
    SensitiveWords, ToolCall, ToolCallDelta, Usage, Utf8Split, build_chat_request, parse_dimension_groups, parse_frame,
    parse_trailer_error, request_log_body, response_log_body, sanitize_system_prompt, sentry_trace, wrap_envelope,
};
use crate::gemini_stream::ClaudeInputTokens;
use crate::kimi_http::{capture_chunk, capture_error, capture_metadata, capture_request};
use crate::openai_compat_payload::ensure_responses_usage_details;
use crate::proxy::{GoClients, GoHeaders, Proxy, default_client, read_all, send};

/// Provider string served by this executor.
pub const PROVIDER: &str = "devin";
/// `maxDevinToolCalls`.
const MAX_TOOL_CALLS: usize = 128;
/// Upstream error bodies are read up to 1 MiB.
const ERROR_BODY_LIMIT: usize = 1 << 20;

pub struct DevinExecutor {
    clients: GoClients,
    matcher: Mutex<Option<(String, Option<Arc<SensitiveWords>>)>>,
    /// Go's runtime-only `Quota.Signals` and `LastRefreshedAt`, per credential.
    runtime: Mutex<HashMap<String, Runtime>>,
}

#[derive(Debug, Clone, Default)]
struct Runtime {
    signals: BTreeMap<String, String>,
    observed_at: Option<std::time::SystemTime>,
    refreshed_at: Option<chrono::DateTime<chrono::Utc>>,
}

impl Default for DevinExecutor {
    fn default() -> Self {
        Self::with_client(default_client())
    }
}

/// `devinAuthCredentials`: session token, base URL and device seed. Attributes win
/// over metadata; a metadata base URL applies only while the base is still the default.
fn creds(c: &Credential) -> (String, String, String) {
    let attr = |k: &str| c.attributes.get(k).map(|v| v.trim().to_owned()).unwrap_or_default();
    let meta = |k: &str| c.str(k).map(|v| v.trim().to_owned()).unwrap_or_default();
    let mut key = String::new();
    for k in ["api_key", "session_token", "token"] {
        if key.is_empty() {
            key = attr(k);
        }
    }
    let mut base = DEFAULT_BASE_URL.to_owned();
    if !attr("base_url").is_empty() {
        base = attr("base_url");
    }
    let mut seed = attr("device_seed");
    for k in ["api_key", "session_token"] {
        if key.is_empty() {
            key = meta(k);
        }
    }
    if base == DEFAULT_BASE_URL && !meta("base_url").is_empty() {
        base = meta("base_url");
    }
    if seed.is_empty() {
        seed = meta("device_seed");
    }
    (key, base, seed)
}

/// `normalizeDevinUUID`: a UUID passes through, anything else becomes its UUIDv5 in
/// the OID namespace, and nothing becomes a random UUID.
pub(crate) fn normalize_uuid(raw: &str) -> String {
    let raw = raw.trim();
    if raw.is_empty() {
        return uuid::Uuid::new_v4().to_string();
    }
    if go_uuid_parses(raw) {
        return raw.to_owned();
    }
    uuid::Uuid::new_v5(&uuid::Uuid::NAMESPACE_OID, raw.as_bytes()).to_string()
}

/// Go `uuid.Parse` acceptance: 36-character hyphenated, `{...}`, `urn:uuid:...`
/// (case-insensitive prefix) or 32 hex digits.
fn go_uuid_parses(s: &str) -> bool {
    let b = s.as_bytes();
    let hyphenated = |b: &[u8]| {
        b.len() == 36
            && b.iter().enumerate().all(|(i, c)| {
                if matches!(i, 8 | 13 | 18 | 23) {
                    *c == b'-'
                } else {
                    c.is_ascii_hexdigit()
                }
            })
    };
    match b.len() {
        36 => hyphenated(b),
        38 => b[0] == b'{' && b[37] == b'}' && hyphenated(&b[1..37]),
        45 => b[..9].eq_ignore_ascii_case(b"urn:uuid:") && hyphenated(&b[9..]),
        32 => b.iter().all(u8::is_ascii_hexdigit),
        _ => false,
    }
}

/// Go's statusErr scope conventions: a 429 cools the model, auth and server faults the
/// credential, anything else is the request's.
fn scope(status: u16) -> FailureScope {
    match status {
        429 => FailureScope::Model,
        401 | 402 | 403 | 408 | 500.. => FailureScope::Credential,
        _ => FailureScope::Request,
    }
}

fn status_error(status: u16, message: impl Into<String>) -> ExecError {
    ExecError::local(status, scope(status), message)
}

/// A plain Go error (no status): the handler answers 500.
fn plain_error(message: impl Into<String>) -> ExecError {
    ExecError::local(500, FailureScope::Transport, message)
}

fn apply_patch_error() -> ExecError {
    ExecError::local(502, FailureScope::Request, cpa_translate::APPLY_PATCH_UPSTREAM_ERROR)
}

/// `newDevinStatusError`: the upstream body, with a 429's `Retry-After` (seconds or an
/// HTTP date) as the cooldown hint.
fn upstream_error(status: u16, headers: http::HeaderMap, body: Bytes) -> ExecError {
    let mut retry_after = None;
    if status == 429
        && let Some(raw) = headers
            .get(http::header::RETRY_AFTER)
            .and_then(|v| v.to_str().ok())
            .map(str::trim)
            .filter(|v| !v.is_empty())
    {
        if let Ok(seconds) = raw.parse::<i64>() {
            if seconds >= 0 {
                retry_after = Some(Duration::from_secs(seconds as u64));
            }
        } else if let Ok(at) = httpdate::parse_http_date(raw) {
            retry_after = at
                .duration_since(std::time::SystemTime::now())
                .ok()
                .filter(|d| !d.is_zero());
        }
    }
    ExecError {
        status,
        scope: scope(status),
        body,
        headers: Box::new(headers),
        retry_after,
        direct: false,
    }
}

/// `opts.OriginalRequest`, falling back to the payload (`ApplyPatchOriginalRequest`).
fn original(req: &ExecRequest) -> &Bytes {
    if req.original_body.is_empty() {
        &req.body
    } else {
        &req.original_body
    }
}

/// ponytail: adapter for `helps.ApplyPatchRequested` / `IsApplyPatchUpstreamTool`
/// (owner: translators; cpa-translate's `reverse_identity_map` is crate-private).
/// Minimal: a top-level custom `apply_patch` tool in the original Responses request (or
/// its `request` wrapper); Go's full resolver also maps namespaced and sanitized names.
fn apply_patch_tool_names(original: &[u8]) -> Vec<Vec<u8>> {
    if original.is_empty() || !gj::valid(original) {
        return Vec::new();
    }
    let mut root = gj::parse(original);
    let req = root.get("request");
    if req.exists() && (req.get("model").exists() || req.get("input").exists() || req.get("tools").exists()) {
        root = req;
    }
    let tools = root.get("tools");
    if !tools.is_array() {
        return Vec::new();
    }
    tools
        .array()
        .iter()
        .filter(|t| {
            *t.get("type").bytes() == *b"custom"
                && cpa_common::gostr::trim_space(&t.get("name").bytes()) == b"apply_patch"
        })
        .map(|_| b"apply_patch".to_vec())
        .collect()
}

fn apply_patch_requested(original: &[u8]) -> bool {
    !apply_patch_tool_names(original).is_empty()
}

fn is_apply_patch_upstream_tool(original: &[u8], name: &[u8]) -> bool {
    apply_patch_tool_names(original).iter().any(|n| n == name)
}

struct Prepared {
    url: String,
    headers: GoHeaders,
    body: Bytes,
    /// The chat model UID (Go `SetUpstreamModel` when non-empty).
    model_uid: String,
    /// `BuildDevinUpstreamLogBody`: what request logging records instead of the
    /// protobuf (built only when capture is on).
    log_body: Vec<u8>,
    /// `devinAuthLogFields`' value: the key's first and last four bytes.
    auth_value: String,
}

/// `devinAuthLogFields`' value for a session key: its first and last four bytes, `***`
/// for a short key, empty without one.
fn auth_log_value(key: &str) -> String {
    match key.len() {
        0 => String::new(),
        1..=8 => "***".into(),
        n => {
            let b = key.as_bytes();
            format!(
                "{}...{}",
                String::from_utf8_lossy(&b[..4]),
                String::from_utf8_lossy(&b[n - 4..])
            )
        }
    }
}

impl DevinExecutor {
    /// Uses a caller-built client (tests point it at local mocks).
    pub fn with_client(client: wreq::Client) -> Self {
        Self {
            clients: GoClients::with_default(client),
            matcher: Mutex::new(None),
            runtime: Mutex::new(HashMap::new()),
        }
    }

    /// `getSensitiveWordMatcher`: `devin.sensitive-words` (v8
    /// `oauth.providers.devin.sensitive-words`), compiled once per word list.
    fn matcher(&self, cfg: &Config) -> Option<Arc<SensitiveWords>> {
        let words: Vec<String> = cfg
            .document
            .get("oauth")
            .and_then(|o| o.get("providers"))
            .and_then(|p| p.get("devin"))
            .and_then(|d| d.get("sensitive-words"))
            .and_then(|w| w.as_sequence())
            .map(|seq| seq.iter().filter_map(|v| v.as_str().map(str::to_owned)).collect())
            .unwrap_or_default();
        if words.is_empty() {
            return None;
        }
        let key = words.join("\0");
        let mut cached = self.matcher.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some((k, m)) = cached.as_ref()
            && *k == key
        {
            return m.clone();
        }
        let built = SensitiveWords::new(&words).map(Arc::new);
        *cached = Some((key, built.clone()));
        built
    }

    /// Go's refresh scheduling for Devin: no SDK lead, so only a `refresh_interval`
    /// (or an expiry inside it) makes the background loop re-read the user status.
    pub fn needs_prepare(&self, credential: &Credential, cfg: &Config) -> bool {
        self.needs_prepare_at(credential, cfg, chrono::Utc::now())
    }

    pub fn needs_prepare_at(&self, credential: &Credential, _cfg: &Config, now: chrono::DateTime<chrono::Utc>) -> bool {
        // Go's refresh loop never schedules API-key-kind credentials.
        if creds(credential).0.is_empty() || cpa_core::registry::dynamic::is_api_key(credential) {
            return false;
        }
        let Some(interval) = crate::kimi_http::preferred_interval(credential) else {
            return false;
        };
        if let Some(expiry) = crate::kimi_http::expiration(credential)
            && (expiry <= now || expiry - now <= interval)
        {
            return true;
        }
        let refreshed = self
            .runtime
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&credential.id)
            .and_then(|r| r.refreshed_at);
        let last = match (crate::kimi_http::last_refresh(credential), refreshed) {
            (Some(a), Some(b)) => Some(a.max(b)),
            (a, b) => a.or(b),
        };
        last.is_none_or(|last| now - last >= interval)
    }

    /// `Refresh`: user status, plan and quota from `GetUserStatus`. Returns the account
    /// fields to persist; quota signals stay in memory ([`Self::quota`]).
    pub async fn prepare(&self, credential: &Credential, cfg: &Config) -> Result<MetadataPatch, ExecError> {
        let (token, base, seed) = creds(credential);
        if token.is_empty() {
            return Ok(MetadataPatch::default());
        }
        let client = self.clients.get(&Proxy::effective(credential, cfg));
        let auth = DevinAuth::new(client).with_devin_transport();
        let auth = if base.is_empty() {
            auth
        } else {
            auth.with_server_base(&base)
        };
        let status = auth.fetch_user_status(&token, &seed).await.map_err(|e| {
            tracing::warn!("devin executor: failed to refresh user status for {}", credential.id);
            ExecError::local(500, FailureScope::Credential, e)
        })?;
        let mut patch = MetadataPatch::default();
        for (key, value) in [
            ("email", &status.email),
            ("user_name", &status.user_name),
            ("user_id", &status.user_id),
            ("team_id", &status.team_id),
            ("plan", &status.plan),
            ("org_id", &status.org_id),
            ("org_name", &status.org_name),
        ] {
            if !value.is_empty() {
                patch.set.insert(key.into(), Value::String(value.clone()));
            }
        }
        let mut runtime = self.runtime.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let entry = runtime.entry(credential.id.clone()).or_default();
        for (key, value) in status.quota_signals() {
            entry.signals.insert(key.into(), value);
        }
        entry.observed_at = Some(std::time::SystemTime::now());
        entry.refreshed_at = Some(chrono::Utc::now());
        Ok(patch)
    }

    /// The credential's quota observation (plan, daily and weekly remaining, resets)
    /// from its last refresh.
    pub fn quota(&self, credential_id: &str) -> Option<Snapshot> {
        let runtime = self.runtime.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let r = runtime.get(credential_id)?;
        Some(Snapshot {
            observed_at: r.observed_at?,
            signals: r.signals.clone(),
        })
    }

    /// Seeds a credential's quota observation (Go's login record carries one).
    pub fn observe_quota(&self, credential_id: &str, signals: BTreeMap<String, String>) {
        let mut runtime = self.runtime.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let entry = runtime.entry(credential_id.to_owned()).or_default();
        entry.signals = signals;
        entry.observed_at = Some(std::time::SystemTime::now());
    }

    /// `prepareDevinHTTPRequest`.
    fn prepare_request(&self, credential: &Credential, req: &ExecRequest, cfg: &Config) -> Result<Prepared, ExecError> {
        let (key, base, seed) = creds(credential);
        if key.is_empty() {
            const MISSING: &str = "devin credentials missing: api_key or session_token required";
            // Go's error is plain: the usage record carries no status.
            req.usage.publish_failure(0, MISSING);
            return Err(ExecError::local(500, FailureScope::Credential, MISSING));
        }
        let payload = if req.source_format == Format::Interactions {
            req.body.to_vec()
        } else {
            cpa_translate::translate_request(
                req.source_format,
                Format::Interactions,
                &cpa_translate::RequestCtx {
                    model: &req.model,
                    stream: req.stream,
                },
                &req.body,
            )
            .map_err(|e| ExecError::local(400, FailureScope::Request, e.0))?
        };
        let parsed = parse_interactions(&payload, &req.original_body);
        let session = String::from_utf8_lossy(&parsed.session_id).into_owned();
        let session = if session.is_empty() {
            cpa_common::session::cpa_session_id(req.session.as_deref()).unwrap_or_default()
        } else {
            session
        };
        let session_id = normalize_uuid(&session);
        let cascade = String::from_utf8_lossy(&parsed.cascade_id).into_owned();
        let cascade_id = if cascade.is_empty() {
            session_id.clone()
        } else {
            normalize_uuid(&cascade)
        };
        let mut max_tokens = parsed.max_tokens;
        let base_model = parse_suffix(&req.model).model_name;
        if let Some(info) = cpa_core::registry::lookup_model(&base_model, Some(PROVIDER)) {
            let limit = info
                .raw
                .get("max_completion_tokens")
                .and_then(Value::as_i64)
                .unwrap_or(0);
            if limit > 0 && (max_tokens > limit || max_tokens <= 0) {
                max_tokens = limit;
            }
        }
        let model_uid = resolve_chat_model_uid(&req.model, &parsed.thinking_level, parsed.budget);
        let matcher = self.matcher(cfg);
        let proto = build_chat_request(&ChatRequest {
            session_token: &key,
            device_seed: &seed,
            model_uid: &model_uid,
            system_prompt: &parsed.system,
            prompts: &parsed.prompts,
            tools: &parsed.tools,
            temperature: parsed.temperature,
            max_tokens,
            session_id: &session_id,
            cascade_id: &cascade_id,
            matcher: matcher.as_deref(),
        });
        let log_body = if req.capture().enabled() {
            let system = sanitize_system_prompt(&parsed.system, matcher.as_deref());
            request_log_body(&RequestLog {
                interactions: &payload,
                interactions_source: req.source_format == Format::Interactions,
                model_uid: &model_uid,
                system_prompt: &system,
                prompts: &parsed.prompts,
                tools: &parsed.tools,
                temperature: parsed.temperature,
                max_tokens,
                session_id: &session_id,
                cascade_id: &cascade_id,
            })
        } else {
            Vec::new()
        };
        Ok(Prepared {
            url: format!("{}{CHAT_PATH}", base.trim_end_matches('/')),
            headers: headers(credential, req, &key),
            body: Bytes::from(wrap_envelope(&proto)),
            model_uid,
            log_body,
            auth_value: auth_log_value(&key),
        })
    }

    pub async fn execute(
        &self,
        credential: &Credential,
        req: ExecRequest,
        cfg: &Config,
    ) -> Result<ExecResponse, ExecError> {
        if req.operation == Operation::CountTokens {
            // Devin has no token counting endpoint: a length estimate.
            let n = req.body.len() / 4;
            return Ok(ExecResponse {
                status: 200,
                headers: http::HeaderMap::new(),
                body: ResponseBody::Buffered(Bytes::from(format!(r#"{{"total_tokens":{n},"input_tokens":{n}}}"#))),
            });
        }
        let prepared = self.prepare_request(credential, &req, cfg)?;
        if !prepared.model_uid.is_empty() {
            req.usage.upstream_model(&prepared.model_uid);
        }
        let client = self.clients.get(&Proxy::effective(credential, cfg));
        let capture = req.capture().clone();
        capture_request(
            &capture,
            credential,
            &prepared.url,
            &prepared.headers,
            &prepared.log_body,
            PROVIDER,
            ("devin", &prepared.auth_value),
        );
        req.usage.round_trip_started();
        let mut upstream = send(&client, &prepared.url, prepared.headers, prepared.body, None)
            .await
            .inspect_err(|e| capture_error(&capture, e))?;
        capture_metadata(&capture, upstream.status, &upstream.headers);
        upstream.body = crate::kimi_http::track_first_byte(upstream.body, &req.usage, false);
        if !(200..300).contains(&upstream.status) {
            let headers = upstream.headers.clone();
            let body = read_all(upstream.body, ERROR_BODY_LIMIT, true)
                .await
                .unwrap_or_default();
            capture_chunk(&capture, &body);
            return Err(upstream_error(upstream.status, headers, body));
        }
        let reader = FrameReader::new(upstream.body);
        let ctx_original = original(&req).clone();
        let response_format = req.response_format;
        let pair = cpa_translate::pair(response_format, Format::Interactions);
        if pair.is_none() && response_format != Format::Interactions {
            return Err(ExecError::local(
                501,
                FailureScope::Request,
                "Devin response translation pair is not registered",
            ));
        }
        let ctx = ResponseCtx {
            model: &req.model,
            original_request: &ctx_original,
            translated_request: &req.body,
        };
        if req.stream {
            let translator = pair.map(|pair| (pair.stream)(&ctx));
            let claude = ClaudeInputTokens::new(
                req.source_format,
                Format::Interactions,
                response_format,
                req.original_body.clone(),
            );
            let stream = DevinStream::new(
                req.model.clone(),
                response_format,
                translator,
                claude,
                reader,
                req.usage.clone(),
                capture,
            );
            return Ok(ExecResponse {
                status: upstream.status,
                headers: upstream.headers,
                body: ResponseBody::Stream(stream.run()),
            });
        }
        let (consumed, log) = consume_frames(reader, &req.model, &ctx_original).await;
        let built = consumed.as_ref().map(|(json, _)| json.as_slice()).unwrap_or_default();
        if capture.logs_responses() && (log.is_some() || !built.is_empty()) {
            capture_chunk(&capture, &response_log_body(log.as_ref(), built));
        }
        let interactions = match consumed {
            Ok((json, usage_model)) => {
                if !usage_model.is_empty() {
                    req.usage.response_model(&String::from_utf8_lossy(&usage_model));
                }
                json
            }
            Err(_) if apply_patch_requested(&ctx_original) => return Err(apply_patch_error()),
            Err(e) => {
                capture_error(&capture, &e);
                return Err(e);
            }
        };
        let out = match pair {
            Some(pair) => (pair.non_stream)(&ctx, &interactions)
                .ok()
                .filter(|out| !out.is_empty())
                .ok_or_else(apply_patch_error)?,
            None => interactions.clone(),
        };
        if req.usage.enabled() {
            req.usage
                .response_body(Format::Interactions, &usage_payload(&interactions, "model"));
        }
        let out = if response_format == Format::OpenAIResponse {
            ensure_responses_usage_details(&out)
        } else {
            out
        };
        Ok(ExecResponse {
            status: upstream.status,
            headers: upstream.headers,
            body: ResponseBody::Buffered(Bytes::from(out)),
        })
    }
}

/// `PrepareRequest`: Connect headers, `Basic <token>-<token>`, a Sentry trace on chat
/// calls, no User-Agent and no automatic gzip; credential custom headers last.
fn headers(credential: &Credential, req: &ExecRequest, key: &str) -> GoHeaders {
    let mut h = GoHeaders::new();
    h.disable_compression();
    if !key.is_empty() {
        h.set("Authorization", format!("Basic {key}-{key}"));
    }
    h.set("Content-Type", "application/connect+proto");
    h.set("Connect-Protocol-Version", "1");
    h.set("Accept", "*/*");
    h.set("Sentry-Trace", sentry_trace());
    h.set("User-Agent", "");
    for (name, value) in crate::kimi_http::credential_headers(credential, req, original(req)) {
        h.set(&name, value);
    }
    h
}

/// The Interactions payload Go parses usage from, without the client-facing model (Go
/// sets the response model only from upstream usage).
fn usage_payload(payload: &[u8], model_path: &str) -> Vec<u8> {
    let mut out = payload.to_vec();
    gj::delete(&mut out, model_path);
    out
}

fn interaction_id() -> String {
    let id = uuid::Uuid::new_v4().to_string();
    format!("interaction_{}", &id[..12])
}

/// Merges one frame's usage and dimension-group counts into the running total.
fn merge_usage(total: &mut Option<Usage>, frame_usage: Option<&Usage>, groups: &[Vec<u8>]) {
    if let Some(u) = frame_usage {
        match total {
            None => *total = Some(u.clone()),
            Some(t) => {
                if u.prompt_tokens > 0 {
                    t.prompt_tokens = u.prompt_tokens;
                }
                if u.completion_tokens > 0 {
                    t.completion_tokens = u.completion_tokens;
                }
                if u.cached_tokens > 0 {
                    t.cached_tokens = u.cached_tokens;
                }
                if u.cache_write_tokens > 0 {
                    t.cache_write_tokens = u.cache_write_tokens;
                }
                if !u.request_id.is_empty() {
                    t.request_id.clone_from(&u.request_id);
                }
                if !u.model_name.is_empty() {
                    t.model_name.clone_from(&u.model_name);
                }
                for (k, v) in &u.headers {
                    t.headers.insert(k.clone(), v.clone());
                }
            }
        }
    }
    let incomplete = total
        .as_ref()
        .is_none_or(|t| t.prompt_tokens == 0 || t.completion_tokens == 0 || t.cached_tokens == 0);
    if !groups.is_empty()
        && incomplete
        && let Some((input, output, cached)) = parse_dimension_groups(groups)
    {
        let t = total.get_or_insert_with(Usage::default);
        if t.prompt_tokens == 0 {
            t.prompt_tokens = input;
        }
        if t.completion_tokens == 0 {
            t.completion_tokens = output;
        }
        if t.cached_tokens == 0 {
            t.cached_tokens = cached;
        }
    }
}

/// Status and finish reason for the last stop reason (1 and 3 are length, 11 is the
/// content filter).
fn completion_status(stop_reason: u64) -> (&'static str, &'static str) {
    match stop_reason {
        1 | 3 => ("incomplete", "length"),
        11 => ("incomplete", "content_filter"),
        _ => ("completed", ""),
    }
}

/// Writes the usage fields the completed interaction carries under `prefix`.
fn set_usage(out: &mut Vec<u8>, prefix: &str, usage: &Usage) {
    let input = usage.prompt_tokens.wrapping_add(usage.cached_tokens);
    gj::set_int(out, &format!("{prefix}.total_input_tokens"), input);
    gj::set_int(out, &format!("{prefix}.total_output_tokens"), usage.completion_tokens);
    gj::set_int(out, &format!("{prefix}.total_cached_tokens"), usage.cached_tokens);
    if usage.cache_write_tokens > 0 {
        gj::set_int(out, &format!("{prefix}.cache_write_tokens"), usage.cache_write_tokens);
    }
    gj::set_int(
        out,
        &format!("{prefix}.total_tokens"),
        input.wrapping_add(usage.completion_tokens),
    );
}

#[derive(Default)]
struct ToolBuilder {
    legacy: bool,
    id: Vec<u8>,
    name: Vec<u8>,
    args: Vec<u8>,
}

/// `consumeDevinFramesToInteractions`: one Interactions response from every frame.
/// Also returns the upstream usage model name (empty when none was reported).
async fn consume_frames(
    mut reader: FrameReader,
    model: &str,
    original: &[u8],
) -> (Result<(Vec<u8>, Vec<u8>), ExecError>, Option<ResponseLog>) {
    let (mut pre_tool, mut post_tool, mut thinking) = (Vec::new(), Vec::new(), Vec::new());
    let (mut has_pre, mut has_post, mut has_thinking) = (false, false, false);
    let mut builders: Vec<ToolBuilder> = Vec::new();
    let mut by_id: HashMap<Vec<u8>, usize> = HashMap::new();
    let mut last: Option<usize> = None;
    let mut usage: Option<Usage> = None;
    let mut signature: Vec<u8> = Vec::new();
    let mut signature_type: Vec<u8> = Vec::new();
    let mut unknown_fields: Vec<i64> = Vec::new();
    let mut frames = 0;
    let mut stop_reason = 0;
    let mut saw_eos = false;
    // Go's respLog for the frames read so far.
    let log = |status: String,
               builders: &[ToolBuilder],
               usage: &Option<Usage>,
               signature: &[u8],
               signature_type: &[u8],
               unknown_fields: &[i64],
               frames: usize,
               content: Vec<u8>,
               thinking: &[u8]| {
        Some(ResponseLog {
            status,
            frames,
            content,
            thinking: thinking.to_vec(),
            signature: signature.to_vec(),
            signature_type: signature_type.to_vec(),
            tool_calls: builders
                .iter()
                .filter(|b| !(b.id.is_empty() && b.name.is_empty() && b.args.is_empty()))
                .map(|b| ToolCall {
                    id: b.id.clone(),
                    name: b.name.clone(),
                    arguments: b.args.clone(),
                })
                .collect(),
            usage: usage.clone(),
            unknown_fields: unknown_fields.to_vec(),
        })
    };
    macro_rules! log_now {
        ($status:expr) => {
            log(
                $status,
                &builders,
                &usage,
                &signature,
                &signature_type,
                &unknown_fields,
                frames,
                [pre_tool.as_slice(), post_tool.as_slice()].concat(),
                &thinking,
            )
        };
    }
    loop {
        let (flag, payload) = match reader.next().await {
            Ok(frame) => frame,
            Err(FrameError::Eof) => break,
            Err(FrameError::Failed(message)) => {
                return (
                    Err(plain_error(message.clone())),
                    log_now!(format!("read_error: {message}")),
                );
            }
        };
        frames += 1;
        if flag & FLAG_END_STREAM != 0 {
            if let Some((code, message)) = parse_trailer_error(&payload) {
                let status = format!("trailer_error({code}): {message}");
                return (Err(status_error(code, message)), log_now!(status));
            }
            saw_eos = true;
            break;
        }
        let (frame, error) = parse_frame(&payload);
        if error.is_some() {
            continue;
        }
        if frame.stop_reason != 0 {
            stop_reason = frame.stop_reason;
        }
        for field in &frame.unknown_fields {
            if !unknown_fields.contains(field) {
                unknown_fields.push(*field);
            }
        }
        merge_usage(&mut usage, frame.usage.as_ref(), &frame.dimension_groups);
        signature.extend_from_slice(&frame.signature);
        if !frame.signature_type.is_empty() {
            signature_type.clone_from(&frame.signature_type);
        }
        if !frame.thinking.is_empty() {
            thinking.extend_from_slice(&frame.thinking);
            has_thinking = true;
        }
        for tc in &frame.tool_calls {
            let chunk = if tc.arguments.is_empty() {
                &tc.invalid_json
            } else {
                &tc.arguments
            };
            let existing = if tc.id.is_empty() {
                last
            } else {
                by_id.get(&tc.id).copied()
            };
            let index = match existing {
                None => {
                    if builders.len() >= MAX_TOOL_CALLS {
                        tracing::warn!("devin executor: total tool calls exceeded max {MAX_TOOL_CALLS}, dropping");
                        continue;
                    }
                    builders.push(ToolBuilder {
                        id: tc.id.clone(),
                        name: tc.name.clone(),
                        ..ToolBuilder::default()
                    });
                    let index = builders.len() - 1;
                    if !tc.id.is_empty() {
                        by_id.insert(tc.id.clone(), index);
                    }
                    last = Some(index);
                    index
                }
                Some(index) => {
                    last = Some(index);
                    let b = &mut builders[index];
                    if b.id.is_empty() && !tc.id.is_empty() {
                        b.id.clone_from(&tc.id);
                        by_id.insert(tc.id.clone(), index);
                    }
                    if !tc.name.is_empty() {
                        b.name.clone_from(&tc.name);
                    }
                    index
                }
            };
            let b = &mut builders[index];
            if tc.arguments.is_empty() && !tc.invalid_json.is_empty() {
                b.legacy = true;
            }
            b.args.extend_from_slice(chunk);
        }
        if !frame.content.is_empty() {
            if builders.is_empty() {
                pre_tool.extend_from_slice(&frame.content);
                has_pre = true;
            } else {
                post_tool.extend_from_slice(&frame.content);
                has_post = true;
            }
        }
    }
    if builders
        .iter()
        .any(|b| b.legacy && is_apply_patch_upstream_tool(original, &b.name))
    {
        return (Err(apply_patch_error()), None);
    }
    if !saw_eos {
        return (
            Err(plain_error(
                "devin upstream stream terminated prematurely before EOS trailer",
            )),
            log_now!("premature_eof_before_eos".into()),
        );
    }
    let response_log = log_now!("completed".into());
    let (status, finish_reason) = completion_status(stop_reason);
    let mut out =
        br#"{"id":"","model":"","status":"completed","steps":[],"usage":{"total_input_tokens":0,"total_output_tokens":0,"total_cached_tokens":0}}"#.to_vec();
    gj::set_str(&mut out, "id", interaction_id());
    gj::set_str(&mut out, "model", model);
    gj::set_str(&mut out, "status", status);
    if !finish_reason.is_empty() {
        gj::set_str(&mut out, "finish_reason", finish_reason);
    }
    let mut steps: Vec<Vec<u8>> = Vec::new();
    if has_thinking || !signature.is_empty() {
        let mut step = br#"{"type":"thought","content":[{"type":"text","text":""}]}"#.to_vec();
        if has_thinking {
            gj::set_str(&mut step, "content.0.text", &thinking);
        } else {
            gj::delete(&mut step, "content");
        }
        if !signature.is_empty() {
            gj::set_str(&mut step, "signature", &signature);
            gj::set_str(&mut step, "thought_signature", &signature);
        }
        steps.push(step);
    }
    let text_step = |text: &[u8]| {
        let mut step = br#"{"type":"model_output","content":[{"type":"text","text":""}]}"#.to_vec();
        gj::set_str(&mut step, "content.0.text", text);
        step
    };
    if has_pre {
        steps.push(text_step(&pre_tool));
    }
    for b in builders
        .iter()
        .filter(|b| !(b.id.is_empty() && b.name.is_empty() && b.args.is_empty()))
    {
        let mut step = br#"{"type":"function_call","name":"","id":"","call_id":"","arguments":{}}"#.to_vec();
        gj::set_str(&mut step, "name", &b.name);
        gj::set_str(&mut step, "id", &b.id);
        gj::set_str(&mut step, "call_id", &b.id);
        if !b.args.is_empty() {
            if gj::std_valid(&b.args) {
                gj::set_raw(&mut step, "arguments", &b.args);
            } else {
                gj::set_str_no_html(&mut step, "arguments", &b.args);
            }
        }
        steps.push(step);
    }
    if has_post {
        steps.push(text_step(&post_tool));
    }
    if !steps.is_empty() {
        gj::set_raw(&mut out, "steps", gj::join(&steps));
    }
    if let Some(u) = &usage {
        set_usage(&mut out, "usage", u);
    }
    (Ok((out, usage.map(|u| u.model_name).unwrap_or_default())), response_log)
}

/// A tool call slot opened by the stream (`devinActiveToolSlot`).
#[derive(Clone)]
struct Slot {
    step: i64,
    id: Vec<u8>,
    name: Vec<u8>,
}

/// Work deferred while a thought step is open (`pendingActions`).
enum Pending {
    Content(Vec<u8>),
    Tool(ToolCallDelta),
}

/// `streamDevinFrames`: the Connect frame stream as Interactions events, translated for
/// the client.
struct DevinStream {
    reader: Option<FrameReader>,
    model: String,
    response_format: Format,
    translator: Option<Box<dyn StreamTranslator>>,
    claude: ClaudeInputTokens,
    out: VecDeque<Result<Bytes, ExecError>>,
    interaction_id: String,
    step_index: i64,
    thought_started: bool,
    content_started: bool,
    slots: BTreeMap<i64, Slot>,
    by_id: HashMap<Vec<u8>, i64>,
    active: Option<i64>,
    tool_calls: usize,
    thinking_buf: Utf8Split,
    content_buf: Utf8Split,
    usage: Option<Usage>,
    thought_step: i64,
    stop_reason: u64,
    pending: Vec<Pending>,
    post_tool: Vec<Vec<u8>>,
    created_sent: bool,
    translation_failed: bool,
    ended: bool,
    usage_sink: cpa_core::exec::UsageSink,
    /// Request logging (Go's `AppendAPIResponseChunk` and `RecordAPIResponseError`).
    capture: cpa_core::exec::CaptureSink,
    /// What Go's end-of-stream summary reports: frames read, thinking, content and the
    /// signature as received.
    summary: ResponseLog,
    /// Response logging was on when the stream started: only then are thinking, content
    /// and the signature copied into `summary`, which would otherwise hold the whole
    /// response for nothing.
    // ponytail: decided once per stream, so turning request-log on mid-stream logs no
    // summary (Go always buffers it); upgrade by re-checking per frame and marking a
    // partial summary.
    log_summary: bool,
    /// No interactions event has been logged yet.
    first_logged_event: bool,
}

/// `{"event_type":"step.stop","index":N}`.
fn stop_event(index: i64) -> Vec<u8> {
    let mut e = br#"{"event_type":"step.stop","index":0}"#.to_vec();
    gj::set_int(&mut e, "index", index);
    e
}

fn start_event(index: i64, step: &[u8]) -> Vec<u8> {
    let mut e = br#"{"event_type":"step.start","index":0,"step":{}}"#.to_vec();
    gj::set_int(&mut e, "index", index);
    gj::set_raw(&mut e, "step", step);
    e
}

fn tool_start_event(index: i64, name: &[u8], id: &[u8]) -> Vec<u8> {
    let mut e = br#"{"event_type":"step.start","index":0,"step":{"type":"function_call","name":"","id":"","call_id":"","arguments":{}}}"#.to_vec();
    gj::set_int(&mut e, "index", index);
    gj::set_str(&mut e, "step.name", name);
    gj::set_str(&mut e, "step.id", id);
    gj::set_str(&mut e, "step.call_id", id);
    e
}

fn text_delta_event(index: i64, text: &[u8]) -> Vec<u8> {
    let mut e = br#"{"event_type":"step.delta","index":0,"delta":{"type":"text","text":""}}"#.to_vec();
    gj::set_int(&mut e, "index", index);
    gj::set_str(&mut e, "delta.text", text);
    e
}

impl DevinStream {
    fn new(
        model: String,
        response_format: Format,
        translator: Option<Box<dyn StreamTranslator>>,
        claude: ClaudeInputTokens,
        reader: FrameReader,
        usage_sink: cpa_core::exec::UsageSink,
        capture: cpa_core::exec::CaptureSink,
    ) -> Self {
        // Go's Devin reporter takes the response model only from upstream usage frames
        // (`response_model`); reporting this neutral line marks the record as
        // executor-fed, so the server never falls back to the client-format frames,
        // which carry the client's model.
        usage_sink.response_line(Format::Interactions, b"{}");
        Self {
            reader: Some(reader),
            model,
            response_format,
            translator,
            claude,
            out: VecDeque::new(),
            interaction_id: interaction_id(),
            step_index: 0,
            thought_started: false,
            content_started: false,
            slots: BTreeMap::new(),
            by_id: HashMap::new(),
            active: None,
            tool_calls: 0,
            thinking_buf: Utf8Split::default(),
            content_buf: Utf8Split::default(),
            usage: None,
            thought_step: -1,
            stop_reason: 0,
            pending: Vec::new(),
            post_tool: Vec::new(),
            created_sent: false,
            translation_failed: false,
            ended: false,
            usage_sink,
            log_summary: capture.logs_responses(),
            capture,
            summary: ResponseLog {
                status: "completed".into(),
                ..ResponseLog::default()
            },
            first_logged_event: true,
        }
    }

    fn run(self) -> ExecStream {
        futures_util::stream::unfold(self, |mut st| async move {
            loop {
                if let Some(item) = st.out.pop_front() {
                    return Some((item, st));
                }
                if st.ended {
                    return None;
                }
                st.step().await;
            }
        })
        .boxed()
    }

    /// Ends the stream with `error` after any client frames still pending.
    fn fail(&mut self, error: ExecError) {
        self.ended = true;
        if let Some(t) = self.translator.as_mut() {
            self.out.extend(t.flush_frames().into_iter().map(Ok));
        }
        self.out.push_back(Err(error));
    }

    /// Client chunks for one translated batch: Responses usage details and the Claude
    /// input-token estimate, unless the translator rejected apply_patch input
    /// (`TranslateStreamWithClaudeInputTokens`).
    fn deliver(&mut self, mut chunks: Vec<Bytes>) {
        let failed = self.translator.as_ref().is_some_and(|t| t.tool_input_failed());
        if !failed {
            if self.response_format == Format::OpenAIResponse {
                for c in &mut chunks {
                    *c = Bytes::from(ensure_responses_usage_details(c));
                }
            }
            self.claude.apply(&mut chunks);
        }
        self.out.extend(chunks.into_iter().map(Ok));
    }

    /// `emitInteractionsEvent`: false once the stream must stop.
    fn emit(&mut self, raw: Vec<u8>) -> bool {
        if self.translation_failed || self.ended {
            return false;
        }
        if raw.is_empty() {
            return true;
        }
        let trimmed = cpa_common::gostr::trim_space(&raw).to_vec();
        let kind = gj::get(&trimmed, "event_type").bytes().into_owned();
        let failed = kind == b"response.failed" || kind == b"interaction.failed";
        // A failure before anything started is answered as an HTTP error instead.
        if failed && !self.created_sent {
            return true;
        }
        if !self.created_sent && kind != b"interaction.created" {
            self.created_sent = true;
            let mut created = br#"{"event_type":"interaction.created","interaction":{"id":"","model":""}}"#.to_vec();
            gj::set_str(&mut created, "interaction.id", &self.interaction_id);
            gj::set_str(&mut created, "interaction.model", &self.model);
            if !self.emit(created) {
                return false;
            }
        }
        if kind == b"interaction.created" {
            self.created_sent = true;
        }
        if self.capture.enabled() {
            if std::mem::take(&mut self.first_logged_event) {
                capture_chunk(&self.capture, b"=== INTERMEDIATE INTERACTIONS STREAM ===\n");
            }
            // Go logs `trimmed + "\n"`.
            capture_chunk(&self.capture, &[trimmed.as_slice(), b"\n"].concat());
        }
        if self.response_format == Format::Interactions {
            let mut frame = b"data: ".to_vec();
            frame.extend_from_slice(&trimmed);
            frame.extend_from_slice(b"\n\n");
            self.out.push_back(Ok(Bytes::from(frame)));
            return true;
        }
        let Some(t) = self.translator.as_mut() else {
            return true;
        };
        // The shared Interactions-upstream adapter reads SSE: one event per blank line.
        match t.event(&[trimmed.as_slice(), b"\n\n"].concat()) {
            Ok(chunks) => self.deliver(chunks),
            Err(e) => {
                self.translation_failed = true;
                self.fail(ExecError::local(502, FailureScope::Request, e.to_string()));
                return false;
            }
        }
        if self.translator.as_ref().is_some_and(|t| t.tool_input_failed()) {
            self.translation_failed = true;
            self.fail(apply_patch_error());
            return false;
        }
        true
    }

    /// `EndApplyPatchStream`: finalize tool input at transport end; true when that
    /// failed the stream.
    fn end_apply_patch(&mut self) -> bool {
        let Some(t) = self.translator.as_mut() else {
            return false;
        };
        let chunks = t.finalize_tool_input();
        let failed = t.tool_input_failed();
        self.out.extend(chunks.into_iter().map(Ok));
        if failed {
            self.translation_failed = true;
            self.fail(apply_patch_error());
        }
        failed
    }

    fn flush_pending(&mut self) -> bool {
        if self.thought_started {
            if !self.emit(stop_event(self.thought_step)) {
                return false;
            }
            self.thought_started = false;
            self.step_index += 1;
        }
        for action in std::mem::take(&mut self.pending) {
            let ok = match action {
                Pending::Content(chunk) => self.emit_content(chunk),
                Pending::Tool(tc) => self.emit_tool_call(&tc),
            };
            if !ok {
                return false;
            }
        }
        true
    }

    fn emit_content(&mut self, chunk: Vec<u8>) -> bool {
        if self.thought_started {
            let index = if self.thought_step < 0 {
                self.step_index
            } else {
                self.thought_step
            };
            if !self.emit(stop_event(index)) {
                return false;
            }
            self.thought_started = false;
            self.step_index += 1;
        }
        if self.tool_calls > 0 {
            self.post_tool.push(chunk);
            return true;
        }
        if !self.content_started {
            if !self.emit(start_event(self.step_index, br#"{"type":"model_output"}"#)) {
                return false;
            }
            self.content_started = true;
        }
        self.emit(text_delta_event(self.step_index, &chunk))
    }

    fn emit_tool_call(&mut self, tc: &ToolCallDelta) -> bool {
        if self.thought_started {
            if !self.emit(stop_event(self.step_index)) {
                return false;
            }
            self.thought_started = false;
            self.step_index += 1;
        }
        if self.content_started {
            if !self.emit(stop_event(self.step_index)) {
                return false;
            }
            self.content_started = false;
            self.step_index += 1;
        }
        let (args, invalid) = if tc.arguments.is_empty() {
            (&tc.invalid_json, !tc.invalid_json.is_empty())
        } else {
            (&tc.arguments, false)
        };
        let slot = if tc.id.is_empty() {
            self.active
        } else {
            self.by_id.get(&tc.id).copied()
        };
        let step = match slot {
            None => {
                if self.tool_calls >= MAX_TOOL_CALLS {
                    tracing::warn!("devin executor: total tool calls exceeded max {MAX_TOOL_CALLS}, dropping");
                    return true;
                }
                self.tool_calls += 1;
                let step = self.step_index;
                self.step_index += 1;
                self.slots.insert(
                    step,
                    Slot {
                        step,
                        id: tc.id.clone(),
                        name: tc.name.clone(),
                    },
                );
                if !tc.id.is_empty() {
                    self.by_id.insert(tc.id.clone(), step);
                }
                self.active = Some(step);
                if !self.emit(tool_start_event(step, &tc.name, &tc.id)) {
                    return false;
                }
                step
            }
            Some(step) => {
                self.active = Some(step);
                let mut update = None;
                if let Some(slot) = self.slots.get_mut(&step) {
                    let mut updated = false;
                    if slot.id.is_empty() && !tc.id.is_empty() {
                        slot.id.clone_from(&tc.id);
                        updated = true;
                    }
                    if slot.name.is_empty() && !tc.name.is_empty() {
                        slot.name.clone_from(&tc.name);
                        updated = true;
                    }
                    if updated {
                        update = Some(slot.clone());
                    }
                }
                if let Some(slot) = update {
                    if !tc.id.is_empty() {
                        self.by_id.insert(tc.id.clone(), step);
                    }
                    if !self.emit(tool_start_event(slot.step, &slot.name, &slot.id)) {
                        return false;
                    }
                }
                step
            }
        };
        if !args.is_empty() {
            let mut delta =
                br#"{"event_type":"step.delta","index":0,"delta":{"type":"arguments_delta","arguments":""}}"#.to_vec();
            gj::set_int(&mut delta, "index", step);
            gj::set_str_no_html(&mut delta, "delta.arguments", args);
            if invalid {
                gj::set_bool(&mut delta, "delta.invalid_json_str", true);
            }
            if !self.emit(delta) {
                return false;
            }
        }
        true
    }

    fn close_open_steps(&mut self) {
        if !self.pending.is_empty() || self.thought_started {
            let _ = self.flush_pending();
        }
        if !self.slots.is_empty() {
            let steps: Vec<i64> = self.slots.keys().copied().collect();
            for step in steps {
                let _ = self.emit(stop_event(step));
            }
            self.slots.clear();
            self.by_id.clear();
            self.active = None;
        }
        if !self.post_tool.is_empty() {
            let _ = self.emit(start_event(self.step_index, br#"{"type":"model_output"}"#));
            self.content_started = true;
            for chunk in std::mem::take(&mut self.post_tool) {
                let _ = self.emit(text_delta_event(self.step_index, &chunk));
            }
        }
        if self.content_started {
            let _ = self.emit(stop_event(self.step_index));
            self.content_started = false;
        }
    }

    /// Reads and handles one frame, or ends the stream.
    async fn step(&mut self) {
        let Some(reader) = self.reader.as_mut() else {
            self.ended = true;
            return;
        };
        let (flag, payload) = match reader.next().await {
            Ok(frame) => frame,
            Err(FrameError::Eof) => return self.finish(None, false),
            Err(FrameError::Failed(message)) => {
                tracing::warn!("devin executor: stream read error: {message}");
                return self.finish(Some(message), false);
            }
        };
        self.summary.frames += 1;
        if flag & FLAG_END_STREAM != 0 {
            match parse_trailer_error(&payload) {
                Some((code, message)) => self.trailer_error(code, message),
                None => self.finish(None, true),
            }
            return;
        }
        let (frame, error) = parse_frame(&payload);
        if error.is_some() {
            return;
        }
        if frame.stop_reason != 0 {
            self.stop_reason = frame.stop_reason;
        }
        merge_usage(&mut self.usage, frame.usage.as_ref(), &frame.dimension_groups);
        if let Some(u) = &frame.usage
            && !u.model_name.is_empty()
        {
            self.usage_sink.response_model(&String::from_utf8_lossy(&u.model_name));
        }
        if self.log_summary {
            self.summary.signature.extend_from_slice(&frame.signature);
            if !frame.signature_type.is_empty() {
                self.summary.signature_type.clone_from(&frame.signature_type);
            }
        }
        if !frame.thinking.is_empty() {
            if !self.pending.is_empty() && !self.flush_pending() {
                return;
            }
            if self.log_summary {
                self.summary.thinking.extend_from_slice(&frame.thinking);
            }
            let chunk = self.thinking_buf.feed(&frame.thinking);
            if !chunk.is_empty() {
                if self.content_started {
                    if !self.emit(stop_event(self.step_index)) {
                        return;
                    }
                    self.content_started = false;
                    self.step_index += 1;
                }
                if !self.thought_started {
                    self.thought_step = self.step_index;
                    if !self.emit(start_event(self.step_index, br#"{"type":"thought"}"#)) {
                        return;
                    }
                    self.thought_started = true;
                }
                let mut delta = br#"{"event_type":"step.delta","index":0,"delta":{"type":"thought_summary","text":"","content":{"type":"text","text":""}}}"#.to_vec();
                gj::set_int(&mut delta, "index", self.thought_step);
                gj::set_str(&mut delta, "delta.text", &chunk);
                gj::set_str(&mut delta, "delta.content.text", &chunk);
                if !self.emit(delta) {
                    return;
                }
            }
        }
        if !frame.signature.is_empty() {
            if self.thought_step == -1 && !self.content_started {
                self.thought_step = self.step_index;
                if !self.emit(start_event(self.step_index, br#"{"type":"thought"}"#)) {
                    return;
                }
                self.thought_started = true;
            }
            let mut delta =
                br#"{"event_type":"step.delta","index":0,"delta":{"type":"thought_signature","signature":""}}"#
                    .to_vec();
            gj::set_int(&mut delta, "index", self.thought_step.max(0));
            gj::set_str(&mut delta, "delta.signature", &frame.signature);
            if !frame.signature_type.is_empty() {
                gj::set_str(&mut delta, "delta.signature_type", &frame.signature_type);
            }
            if !self.emit(delta) {
                return;
            }
        }
        for tc in frame.tool_calls {
            if self.thought_started {
                self.pending.push(Pending::Tool(tc));
            } else if !self.emit_tool_call(&tc) {
                return;
            }
        }
        if !frame.content.is_empty() {
            if self.log_summary {
                self.summary.content.extend_from_slice(&frame.content);
            }
            let chunk = self.content_buf.feed(&frame.content);
            if !chunk.is_empty() {
                if self.thought_started {
                    self.pending.push(Pending::Content(chunk));
                } else {
                    let _ = self.emit_content(chunk);
                }
            }
        }
    }

    /// An EOS trailer carrying an error: open steps close, `response.failed`, then the
    /// mapped HTTP error.
    fn trailer_error(&mut self, code: u16, message: String) {
        self.reader = None;
        if self.end_apply_patch() {
            return;
        }
        self.close_open_steps();
        if self.translation_failed || self.ended {
            self.ended = true;
            return;
        }
        // Go: reporter.PublishFailure(errTrailer), a plain error: no status.
        self.usage_sink.publish_failure(0, &message);
        tracing::warn!("devin executor: trailer error ({code}): {message}");
        self.capture
            .record(cpa_core::exec::CaptureEvent::ResponseError(&message));
        let mut failed = br#"{"event_type":"response.failed","error":{"message":"","code":""}}"#.to_vec();
        gj::set_str(&mut failed, "error.message", &message);
        gj::set_str(&mut failed, "error.code", code.to_string());
        let _ = self.emit(failed);
        if !self.ended {
            self.fail(status_error(code, message));
        }
    }

    /// The end of the frame stream: EOS (`saw_eos`), EOF, or a read error.
    fn finish(&mut self, read_error: Option<String>, saw_eos: bool) {
        self.reader = None;
        if (!saw_eos || read_error.is_some()) && self.end_apply_patch() {
            return;
        }
        self.close_open_steps();
        if self.translation_failed || self.ended {
            self.ended = true;
            return;
        }
        // A read error or a missing EOS returns without publishing: Go's deferred
        // EnsurePublished records a success without usage, not the stream error.
        if read_error.is_some() || !saw_eos {
            self.usage_sink.publish();
        }
        if let Some(message) = &read_error {
            self.capture
                .record(cpa_core::exec::CaptureEvent::ResponseError(message));
        } else if !saw_eos {
            self.capture.record(cpa_core::exec::CaptureEvent::ResponseError(
                "devin stream terminated prematurely before EOS trailer",
            ));
        }
        if let Some(message) = read_error {
            let mut failed =
                br#"{"event_type":"response.failed","error":{"message":"","code":"stream_read_error"}}"#.to_vec();
            gj::set_str(&mut failed, "error.message", &message);
            let _ = self.emit(failed);
            if !self.ended {
                self.fail(plain_error(message));
            }
            return;
        }
        if !saw_eos {
            const TRUNCATED: &str = "devin stream terminated prematurely before EOS trailer";
            let failed = br#"{"event_type":"response.failed","error":{"message":"devin stream terminated prematurely before EOS trailer","code":"stream_truncated"}}"#.to_vec();
            let _ = self.emit(failed);
            if !self.ended {
                self.fail(plain_error(TRUNCATED));
            }
            return;
        }
        let (status, finish_reason) = completion_status(self.stop_reason);
        let mut completed = br#"{"event_type":"interaction.completed","interaction":{"id":"","model":"","status":"completed","usage":{"total_input_tokens":0,"total_output_tokens":0,"total_cached_tokens":0}}}"#.to_vec();
        gj::set_str(&mut completed, "interaction.id", &self.interaction_id);
        gj::set_str(&mut completed, "interaction.model", &self.model);
        gj::set_str(&mut completed, "interaction.status", status);
        if !finish_reason.is_empty() {
            gj::set_str(&mut completed, "interaction.finish_reason", finish_reason);
        }
        if let Some(u) = self.usage.clone() {
            set_usage(&mut completed, "interaction.usage", &u);
        }
        let reported = self
            .usage_sink
            .enabled()
            .then(|| usage_payload(&completed, "interaction.model"));
        if !self.emit(completed) {
            self.ended = true;
            return;
        }
        // Go publishes ParseInteractionsStreamUsage(completed event) once it is sent.
        if let Some(payload) = reported {
            self.usage_sink.response_line(Format::Interactions, &payload);
        }
        // Go's response summary: the signature as received (not reformatted), no tool
        // calls.
        if self.log_summary && (self.usage.is_some() || !self.summary.signature.is_empty()) {
            let summary = ResponseLog {
                usage: self.usage.clone(),
                ..std::mem::take(&mut self.summary)
            };
            let mut chunk = b"\n=== DEVIN UPSTREAM RESPONSE SUMMARY ===\n".to_vec();
            chunk.extend_from_slice(&summary.marshal_indent());
            chunk.push(b'\n');
            capture_chunk(&self.capture, &chunk);
        }
        self.ended = true;
        if self.response_format == Format::Interactions {
            self.out.push_back(Ok(Bytes::from_static(b"data: [DONE]\n\n")));
            return;
        }
        // The synthetic [DONE] goes through the translator like an event.
        let Some(t) = self.translator.as_mut() else {
            return;
        };
        if let Ok(chunks) = t.event(b"[DONE]\n\n") {
            self.deliver(chunks);
        }
        if let Some(t) = self.translator.as_mut()
            && let Ok(chunks) = t.finish()
        {
            self.out.extend(chunks.into_iter().map(Ok));
        }
    }
}

#[cfg(test)]
#[path = "devin_tests.rs"]
mod tests;
