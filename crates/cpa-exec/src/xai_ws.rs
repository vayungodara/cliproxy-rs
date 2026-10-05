//! xAI upstream Responses WebSocket (xai_websockets_executor.go): the transport a
//! downstream Responses WebSocket turn uses when its credential sets `websockets`
//! (`XAIAutoExecutor`).
//!
//! Sockets live in the Codex executor's pool type ([`codex_ws::Pool`]), as Go shares
//! `codexWebsocketSession` between both executors; xAI keeps its own pool instance. On top
//! of that, every session has an [`IdState`] (`xaiWebsocketIDState`): xAI reuses response
//! IDs and loses upstream state when the socket's target changes, so downstream response
//! IDs are rewritten to stay unique, the conversation transcript is kept to replay it on a
//! fresh target, and a `compaction_trigger` turn compacts that transcript over HTTP.
// ponytail: Go's branch for a WebSocket turn without a downstream WebSocket (SSE encoding
// and client translation) is unreachable through `XAIAutoExecutor` and not ported, nor
// are the sessionless ephemeral sockets or `CloseXAIWebsocketSessionsForAuthID`.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use bytes::Bytes;
use cpa_common::json::{self as gj, GoValue, Kind};
use cpa_core::config::Config;
use cpa_core::credential::Credential;
use cpa_core::exec::{
    CaptureEvent, CaptureSink, ExecError, ExecRequest, ExecResponse, ExecSession, FailureScope, ResponseBody,
    UpstreamRequest,
};
use cpa_core::format::Format;
use futures_util::StreamExt;
use http::{HeaderMap, HeaderName, HeaderValue};
use tokio::sync::{OwnedMutexGuard, mpsc};

use crate::codex_ws::{self, Pool, Read, Session, Target, Upstream};
use crate::openai_compat::status_err;
use crate::openai_compat_payload::ensure_responses_usage_details;
use crate::xai::{XaiExecutor, apply_patch_error, compaction_frames, compaction_item, compaction_response_id, creds};
use crate::xai_auth::DEFAULT_API_BASE_URL;
use crate::xai_replay::{self as replay, ReplayScope};
use crate::xai_request as request;
use crate::xai_response::{self as response, NamespaceRestorer, OutputItems, XSearchFilter, text};
use cpa_translate::apply_patch_responses as apply_patch;

/// The terminal events whose usage Go's WebSocket turn observes (completed and done).
const WS_USAGE_EVENTS: [&str; 2] = ["response.completed", "response.done"];

/// `codexResponsesWebsocketHandshakeTO`.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(30);
/// Bound on a handshake rejection body.
const MAX_HANDSHAKE_BODY: usize = 64 * 1024;
/// `buildXAIWebsocketWarmupCompletedPayload`'s empty usage.
const EMPTY_USAGE: &str = r#"{"input_tokens":0,"input_tokens_details":{"cached_tokens":0},"output_tokens":0,"output_tokens_details":{"reasoning_tokens":0},"total_tokens":0}"#;

/// Go reads xAI sockets without a deadline (only the Codex reader sets one); the shared
/// reader's deadline is set past any session.
const NO_READ_DEADLINE: Duration = Duration::from_secs(10 * 365 * 24 * 3600);

/// The executor's sockets and per-session ID state (`globalXAIWebsocketSessionStore`,
/// `globalXAIWebsocketIDStates`).
pub(crate) struct Ws {
    pool: Pool,
    states: Mutex<HashMap<String, Arc<IdState>>>,
}

impl Default for Ws {
    fn default() -> Self {
        let mut pool = Pool::default();
        pool.idle = NO_READ_DEADLINE;
        Self {
            pool,
            states: Mutex::default(),
        }
    }
}

impl Ws {
    /// `getXAIWebsocketIDState`.
    fn state(&self, id: &str) -> Option<Arc<IdState>> {
        let id = id.trim();
        if id.is_empty() {
            return None;
        }
        let mut states = self.states.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        Some(states.entry(id.to_owned()).or_default().clone())
    }

    /// `CloseExecutionSession`: the downstream connection ended.
    pub(crate) fn close(&self, id: &str) {
        let id = id.trim();
        self.pool.close(id);
        self.states
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(id);
    }

    /// `UpstreamDisconnectChan`.
    pub(crate) fn closed(&self, id: &str) -> impl std::future::Future<Output = ExecError> + Send + 'static {
        self.pool.closed(id.trim())
    }
}

// --- per-session ID state (xaiWebsocketIDState) --------------------------------------------

#[derive(Default)]
struct IdState(Mutex<IdInner>);

#[derive(Default)]
struct IdInner {
    downstream_to_upstream: HashMap<String, String>,
    sequence: u64,
    transcript: Vec<Vec<u8>>,
    replay_compacted_on_reset: bool,
    /// The target of the session's last socket (Go keeps `sess.authID`, `wsURL` and
    /// `proxyURL` after the socket is gone).
    last_target: Option<Target>,
}

impl IdState {
    fn lock(&self) -> std::sync::MutexGuard<'_, IdInner> {
        self.0.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// `upstreamIDForDownstream`.
    fn upstream_id(&self, downstream: &str) -> String {
        let downstream = downstream.trim();
        if downstream.is_empty() {
            return String::new();
        }
        match self.lock().downstream_to_upstream.get(downstream) {
            Some(upstream) => upstream.trim().to_owned(),
            None => downstream.to_owned(),
        }
    }

    /// `mapDownstreamToUpstream`.
    fn map(&self, downstream: &str, upstream: &str) {
        let downstream = downstream.trim();
        if downstream.is_empty() {
            return;
        }
        self.lock()
            .downstream_to_upstream
            .insert(downstream.to_owned(), upstream.trim().to_owned());
    }

    /// `snapshotTranscriptInput`.
    fn snapshot(&self) -> Option<Vec<u8>> {
        let inner = self.lock();
        (!inner.transcript.is_empty()).then(|| raw_array(&inner.transcript))
    }

    /// The transcript prepended to `payload`'s input.
    fn prepend(&self, payload: Vec<u8>, only_compacted: bool) -> (Vec<u8>, bool) {
        let prefix = {
            let inner = self.lock();
            if only_compacted && !inner.replay_compacted_on_reset {
                return (payload, false);
            }
            inner.transcript.clone()
        };
        if prefix.is_empty() {
            return (payload, false);
        }
        let mut merged = prefix;
        merged.extend(raw_items(&gj::get(&payload, "input")));
        let mut out = payload;
        gj::set_raw(&mut out, "input", raw_array(&merged));
        (out, true)
    }

    /// `recordTranscriptTurn`.
    fn record(&self, request: &[u8], completed: &[u8], reset: bool) {
        if request.is_empty() || completed.is_empty() {
            return;
        }
        let input = raw_items(&gj::get(request, "input"));
        let output = raw_items(&gj::get(completed, "response.output"));
        let mut inner = self.lock();
        if reset {
            inner.transcript.clear();
            inner.replay_compacted_on_reset = false;
        }
        inner.transcript.extend(input);
        inner.transcript.extend(output);
    }

    /// `replaceTranscriptWithItems`.
    fn replace(&self, items: &[&[u8]]) {
        let next: Vec<Vec<u8>> = items
            .iter()
            .map(|item| cpa_common::gostr::trim_space(item))
            .filter(|item| !item.is_empty() && gj::std_valid(item))
            .map(<[u8]>::to_vec)
            .collect();
        let mut inner = self.lock();
        inner.replay_compacted_on_reset = !next.is_empty();
        inner.transcript = next;
    }

    /// `websocketSessionTargetChanged`.
    fn target_changed(&self, target: &Target) -> bool {
        self.lock().last_target.as_ref().is_some_and(|last| last != target)
    }

    fn set_target(&self, target: &Target) {
        self.lock().last_target = Some(target.clone());
    }
}

/// `xaiJSONRawMessages`: the valid elements of an array, trimmed.
fn raw_items(value: &gj::Res<'_>) -> Vec<Vec<u8>> {
    if !value.exists() || !value.is_array() {
        return Vec::new();
    }
    value
        .array()
        .iter()
        .map(|item| cpa_common::gostr::trim_space(item.raw()).to_vec())
        .filter(|raw| !raw.is_empty() && gj::std_valid(raw))
        .collect()
}

/// `xaiMarshalRawMessages`.
fn raw_array(items: &[Vec<u8>]) -> Vec<u8> {
    let mut out = vec![b'['];
    for (i, item) in items.iter().enumerate() {
        if i > 0 {
            out.push(b',');
        }
        out.extend_from_slice(cpa_common::gostr::trim_space(item));
    }
    out.push(b']');
    out
}

/// `xaiWebsocketRequestIDMapper`: one turn's view of the session's ID mapping.
struct IdMapper {
    state: Arc<IdState>,
    downstream_previous: String,
    upstream_previous: String,
    upstream_response: String,
    downstream_response: String,
    replayed_compacted: bool,
}

impl IdMapper {
    /// `newXAIWebsocketRequestIDMapper`.
    fn new(state: Arc<IdState>, downstream_request: &[u8]) -> Self {
        let downstream_previous = text(&gj::get(downstream_request, "previous_response_id"))
            .trim()
            .to_owned();
        let upstream_previous = if downstream_previous.is_empty() {
            String::new()
        } else {
            state.upstream_id(&downstream_previous)
        };
        Self {
            state,
            downstream_previous,
            upstream_previous,
            upstream_response: String::new(),
            downstream_response: String::new(),
            replayed_compacted: false,
        }
    }

    /// `upstreamRequestPayload`.
    fn upstream_request(&mut self, payload: Vec<u8>) -> Vec<u8> {
        if payload.is_empty() {
            return payload;
        }
        if self.downstream_previous == self.upstream_previous {
            let request_type = text(&gj::get(&payload, "type"));
            if self.downstream_previous.is_empty() && request_type.trim() == "response.append" {
                let (out, replayed) = self.state.prepend(payload, true);
                self.replayed_compacted = replayed;
                return out;
            }
            return payload;
        }
        let mut out = payload;
        if self.upstream_previous.is_empty() {
            gj::delete(&mut out, "previous_response_id");
            if !self.downstream_previous.is_empty() {
                out = self.state.prepend(out, false).0;
                self.replayed_compacted = true;
            }
            return out;
        }
        gj::set_str(&mut out, "previous_response_id", &self.upstream_previous);
        out
    }

    /// `downstreamResponsePayload`.
    fn downstream_response(&mut self, payload: Vec<u8>) -> Vec<u8> {
        if payload.is_empty() {
            return payload;
        }
        let upstream = text(&gj::get(&payload, "response.id")).trim().to_owned();
        let downstream = self.downstream_id(&upstream);
        if downstream.is_empty() {
            return payload;
        }
        rewrite_ids(
            payload,
            &self.upstream_response,
            &downstream,
            &self.upstream_previous,
            &self.downstream_previous,
        )
    }

    /// `downstreamIDForUpstreamResponse`: a response ID xAI repeats gets a `-xai-N` suffix
    /// downstream.
    fn downstream_id(&mut self, upstream: &str) -> String {
        if !self.upstream_response.is_empty() {
            return self.downstream_response.clone();
        }
        if upstream.is_empty() {
            return String::new();
        }
        let mut inner = self.state.lock();
        self.upstream_response = upstream.to_owned();
        self.downstream_response = upstream.to_owned();
        let seen = inner.downstream_to_upstream.contains_key(upstream);
        if (!self.downstream_previous.is_empty()
            && !self.upstream_previous.is_empty()
            && upstream == self.upstream_previous)
            || seen
        {
            inner.sequence += 1;
            self.downstream_response = format!("{upstream}-xai-{}", inner.sequence);
        }
        inner
            .downstream_to_upstream
            .insert(upstream.to_owned(), upstream.to_owned());
        inner
            .downstream_to_upstream
            .insert(self.downstream_response.clone(), upstream.to_owned());
        self.downstream_response.clone()
    }
}

/// `rewriteXAIWebsocketDownstreamIDs`: `id`/`item_id` strings containing the upstream
/// response ID and an exact `previous_response_id`, re-marshalled like Go's
/// `map[string]any` (sorted keys) when anything changed.
fn rewrite_ids(
    payload: Vec<u8>,
    upstream: &str,
    downstream: &str,
    upstream_previous: &str,
    downstream_previous: &str,
) -> Vec<u8> {
    let (upstream, downstream) = (upstream.trim(), downstream.trim());
    let (upstream_previous, downstream_previous) = (upstream_previous.trim(), downstream_previous.trim());
    if upstream == downstream && upstream_previous == downstream_previous {
        return payload;
    }
    let Some(mut value) = GoValue::parse(&payload) else {
        return payload;
    };
    let ids = Ids {
        upstream,
        downstream,
        upstream_previous,
        downstream_previous,
    };
    if !ids.rewrite(&mut value) {
        return payload;
    }
    value.marshal()
}

struct Ids<'a> {
    upstream: &'a str,
    downstream: &'a str,
    upstream_previous: &'a str,
    downstream_previous: &'a str,
}

impl Ids<'_> {
    fn rewrite(&self, value: &mut GoValue) -> bool {
        match value {
            GoValue::Object(map) => {
                let mut changed = false;
                for (key, child) in map.iter_mut() {
                    if let GoValue::String(s) = child {
                        if let Some(replaced) = self.string(key, s) {
                            *s = replaced;
                            changed = true;
                        }
                        continue;
                    }
                    changed |= self.rewrite(child);
                }
                changed
            }
            GoValue::Array(items) => {
                let mut changed = false;
                for item in items {
                    changed |= self.rewrite(item);
                }
                changed
            }
            _ => false,
        }
    }

    /// `rewriteXAIWebsocketDownstreamIDString`; `None` when unchanged.
    fn string(&self, key: &str, value: &str) -> Option<String> {
        match key {
            "id" | "item_id"
                if !self.upstream.is_empty()
                    && !self.downstream.is_empty()
                    && self.downstream != self.upstream
                    && value.contains(self.upstream) =>
            {
                Some(value.replace(self.upstream, self.downstream)).filter(|r| r != value)
            }
            "previous_response_id"
                if !self.upstream_previous.is_empty()
                    && !self.downstream_previous.is_empty()
                    && value == self.upstream_previous
                    && self.downstream_previous != value =>
            {
                Some(self.downstream_previous.to_owned())
            }
            _ => None,
        }
    }
}

// --- request and frame helpers ----------------------------------------------------------

/// `xaiWebsocketsEnabled`: the `websockets` attribute when it parses, else the metadata
/// value (a bool, or a string that parses).
pub(crate) fn websockets_enabled(credential: &Credential) -> bool {
    if let Some(parsed) = credential
        .attributes
        .get("websockets")
        .map(|v| v.trim())
        .filter(|v| !v.is_empty())
        .and_then(crate::xai::parse_bool)
    {
        return parsed;
    }
    match credential.metadata.get("websockets") {
        Some(serde_json::Value::Bool(b)) => *b,
        Some(serde_json::Value::String(s)) => crate::xai::parse_bool(s.trim()).unwrap_or(false),
        _ => false,
    }
}

/// A plain Go error (`fmt.Errorf`), as the manager sees it.
fn plain(message: impl Into<String>) -> ExecError {
    crate::openai_compat::plain_err(message.into())
}

/// `buildXAIResponsesWebsocketURL`.
// ponytail: Go re-serializes the parsed URL (`URL.String()`); the text after the scheme is
// kept as given, which differs only for escapes Go would normalize.
fn ws_url(http_url: &str) -> Result<String, ExecError> {
    let raw = http_url.trim();
    let parsed = crate::xai_url::parse(raw).map_err(plain)?;
    let scheme = match parsed.scheme.as_str() {
        "http" => "ws",
        "https" => "wss",
        "ws" | "wss" => parsed.scheme.as_str(),
        other => {
            return Err(plain(format!(
                "xai websockets executor: unsupported responses websocket URL scheme {}",
                cpa_common::gostr::quote(other)
            )));
        }
    };
    // Go's `URL.Host` is the hostname with its port.
    if parsed.hostname.trim().is_empty() && parsed.port.is_empty() {
        return Err(plain("xai websockets executor: responses websocket URL host is empty"));
    }
    let rest = &raw[raw.find(':').map_or(0, |i| i + 1)..];
    Ok(format!("{scheme}:{rest}"))
}

/// `buildXAIWebsocketRequestBody`: every turn is a stored `response.create`.
fn request_frame(body: &[u8]) -> Vec<u8> {
    if body.is_empty() {
        return Vec::new();
    }
    let mut out = body.to_vec();
    gj::set_str(&mut out, "type", "response.create");
    for key in ["stream", "stream_options", "background"] {
        gj::delete(&mut out, key);
    }
    gj::set_bool(&mut out, "store", true);
    if !text(&gj::get(&out, "previous_response_id")).trim().is_empty() {
        gj::delete(&mut out, "instructions");
    }
    out
}

/// `xaiWebsocketGenerateFalse`: a warmup turn.
fn generate_false(payload: &[u8]) -> bool {
    let generate = gj::get(payload, "generate");
    generate.exists() && !generate.bool()
}

/// `buildXAIWebsocketWarmupCompletedPayload`: the `response.completed` a warmup turn
/// never receives, built from its `response.created`.
fn warmup_completed(created: &[u8]) -> Vec<u8> {
    let mut completed =
        format!(r#"{{"type":"response.completed","response":{{"output":[],"usage":{EMPTY_USAGE}}}}}"#).into_bytes();
    let sequence = gj::get(created, "sequence_number");
    if sequence.exists() {
        gj::set_int(&mut completed, "sequence_number", sequence.int() + 1);
    }
    let response = gj::get(created, "response");
    if response.exists() && response.is_object() {
        let mut inner = response.raw().to_vec();
        gj::set_str(&mut inner, "status", "completed");
        if !gj::get(&inner, "output").exists() {
            gj::set_raw(&mut inner, "output", "[]");
        }
        if !gj::get(&inner, "usage").exists() {
            gj::set_raw(&mut inner, "usage", EMPTY_USAGE);
        }
        gj::set_raw(&mut completed, "response", inner);
    }
    ensure_responses_usage_details(&completed)
}

/// `parseXAIWebsocketError`: an `error` frame classified like Codex's, then remapped by
/// `xaiStatusErr` (403 bad credentials to 401, the free-usage retry hint); a bare frame
/// with an `error` field takes its status from the frame.
fn ws_error(payload: &[u8]) -> Option<ExecError> {
    let text_payload = String::from_utf8_lossy(payload);
    if let Some(mut error) = codex_ws::ws_error(&text_payload, false) {
        let xai = response::status_error(error.status, payload);
        error.status = xai.status;
        if xai.retry_after.is_some() {
            error.retry_after = xai.retry_after;
        }
        return Some(error);
    }
    if payload.is_empty() || !gj::get(payload, "error").exists() {
        return None;
    }
    let mut status = gj::get(payload, "status").int();
    if status <= 0 {
        status = gj::get(payload, "status_code").int();
    }
    if status <= 0 {
        status = bare_error_status(payload);
    }
    let mut out = b"{}".to_vec();
    gj::set_str(&mut out, "type", "error");
    gj::set_int(&mut out, "status", status);
    let node = gj::get(payload, "error");
    if node.exists() {
        gj::set_raw(&mut out, "error", node.raw());
    }
    // Go keeps any int status; ExecError carries u16 (a larger code reads as 500).
    Some(response::status_error(u16::try_from(status).unwrap_or(500), &out))
}

/// `xaiBareWebsocketErrorStatus`.
fn bare_error_status(payload: &[u8]) -> i64 {
    for path in ["error.code", "error.status", "code"] {
        let raw = text(&gj::get(payload, path)).trim().to_owned();
        if raw.is_empty() {
            continue;
        }
        if let Some(status) = go_atoi(&raw).filter(|s| *s > 0) {
            return status;
        }
    }
    let message = text(&gj::get(payload, "error.message"));
    if message.contains(r#""code":"400""#) || message.contains("Request validation error") {
        return 400;
    }
    500
}

/// `strconv.Atoi`.
fn go_atoi(s: &str) -> Option<i64> {
    let digits = s.strip_prefix(['+', '-']).unwrap_or(s);
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    s.parse().ok()
}

/// A socket failure from the shared pool, as Go's xAI executor reports it: a close 1009
/// is the request-scoped 413 with `mapCodexWebsocketReadError`'s fixed body, and every
/// other read or write failure is a plain Go error (500) under the xAI executor's name.
// ponytail: Go's message for other closes is gorilla's ("websocket: close 1011 (internal
// server error): internal"); the shared reader keeps no close details, so the text is
// the pool's.
fn pool_error(mut error: ExecError) -> ExecError {
    const CODEX: &[u8] = b"codex websockets executor: ";
    if error.status == 413 && error.scope == FailureScope::Request {
        error.body = Bytes::from_static(
            br#"{"error":{"message":"upstream websocket message too big","type":"invalid_request_error","code":"message_too_big"}}"#,
        );
        return error;
    }
    if error.body.starts_with(CODEX) {
        error.body = Bytes::from([&b"xai websockets executor: "[..], &error.body[CODEX.len()..]].concat());
    }
    if error.scope == FailureScope::Transport {
        error.status = 500;
    }
    error
}

/// `shouldRetryXAIWebsocketSend`: a request-scoped failure (413) never retries.
fn should_retry_send(error: &ExecError) -> bool {
    error.scope != FailureScope::Request
}

/// `applyXAIWebsocketHeaders`.
fn handshake_headers(credential: &Credential, token: &str, session_id: &str, req: &ExecRequest) -> HeaderMap {
    let mut headers = HeaderMap::new();
    let mut set = |name: &str, value: &str| {
        if let (Ok(name), Ok(value)) = (HeaderName::from_bytes(name.as_bytes()), HeaderValue::from_str(value)) {
            headers.insert(name, value);
        }
    };
    set("content-type", "application/json");
    if !token.trim().is_empty() {
        set("authorization", &format!("Bearer {token}"));
    }
    if !session_id.is_empty() {
        set("x-grok-conv-id", session_id);
    }
    let session = cpa_common::session::cpa_session_id(req.session.as_deref());
    for (name, value) in cpa_common::headers::custom_headers(&credential.attributes, &req.headers, session.as_deref()) {
        set(&name, &value);
    }
    headers
}

// --- compaction (executeCompactionTriggerFromWebsocketContext) ---------------------------

/// `buildXAIWebsocketCompactionPayload`.
fn compaction_payload(payload: &[u8], transcript: &[u8]) -> Vec<u8> {
    let mut out = if payload.is_empty() {
        b"{}".to_vec()
    } else {
        payload.to_vec()
    };
    gj::set_raw(
        &mut out,
        "input",
        if transcript.is_empty() { &b"[]"[..] } else { transcript },
    );
    gj::delete(&mut out, "previous_response_id");
    out
}

/// `validateXAIWebsocketCompactionResponse`: the normalized response ID and the
/// compaction item to keep as the transcript.
fn validate_compaction(data: &[u8]) -> Result<(String, Vec<u8>), ExecError> {
    let missing = || status_err(502, "xai websocket compaction response is missing compacted state");
    if data.is_empty() || !gj::std_valid(data) {
        return Err(status_err(502, "xai websocket compaction returned invalid JSON"));
    }
    let id = gj::get(data, "id");
    let output = gj::get(data, "output");
    if id.kind != Kind::String || id.str().trim().is_empty() || !output.exists() || !output.is_array() {
        return Err(missing());
    }
    let items = output.array();
    let Some(item) = items.first() else {
        return Err(missing());
    };
    let item_type = item.get("type");
    let encrypted = item.get("encrypted_content");
    if item.kind != Kind::Json
        || item_type.kind != Kind::String
        || item_type.str().trim() != "compaction"
        || encrypted.kind != Kind::String
        || encrypted.str().trim().is_empty()
    {
        return Err(missing());
    }
    let response_id = compaction_response_id(data, SystemTime::now());
    let item = compaction_item(data, &response_id);
    Ok((response_id, item))
}

// --- capture (RecordAPIWebsocket*) ------------------------------------------------------

/// The upstream WebSocket's capture: the request log Go builds once per turn (`wsReqLog`)
/// and the events of its socket. A no-op without an observer.
struct WsCapture {
    sink: CaptureSink,
    url: String,
    headers: Vec<(String, String)>,
    frame: Vec<u8>,
    auth_id: String,
    auth_label: String,
    auth_type: &'static str,
    auth_value: String,
}

impl WsCapture {
    fn new(req: &ExecRequest, credential: &Credential, url: &str, headers: &HeaderMap, frame: &[u8]) -> Self {
        let sink = req.capture().clone();
        if !sink.enabled() {
            // Nothing reads the fields without an observer: keep no copy of the frame.
            return Self {
                sink,
                url: String::new(),
                headers: Vec::new(),
                frame: Vec::new(),
                auth_id: String::new(),
                auth_label: String::new(),
                auth_type: "",
                auth_value: String::new(),
            };
        }
        let (auth_type, auth_value) = crate::openai_compat_http::account_info(credential);
        Self {
            headers: crate::openai_compat_http::header_pairs(headers),
            sink,
            url: url.to_owned(),
            frame: frame.to_vec(),
            auth_id: credential.id.clone(),
            auth_label: credential.label.clone(),
            auth_type,
            auth_value,
        }
    }

    fn log<'a>(
        &'a self,
        url: &'a str,
        method: &'a str,
        headers: &'a [(String, String)],
        body: &'a [u8],
    ) -> UpstreamRequest<'a> {
        UpstreamRequest {
            url,
            method,
            headers,
            body,
            provider: crate::xai::PROVIDER,
            auth_id: &self.auth_id,
            auth_label: &self.auth_label,
            auth_type: self.auth_type,
            auth_value: &self.auth_value,
        }
    }

    /// `RecordAPIWebsocketRequest`.
    fn request(&self) {
        if self.sink.enabled() {
            self.sink.record(CaptureEvent::WebsocketRequest(self.log(
                &self.url,
                "WEBSOCKET",
                &self.headers,
                &self.frame,
            )));
        }
    }

    /// `RecordAPIWebsocketUpgradeRejection`: an HTTP attempt (`websocketUpgradeRequestLog`:
    /// GET on the http(s) URL, no body, Connection and Upgrade defaulted).
    fn rejection(&self, status: u16, headers: &HeaderMap, body: &[u8]) {
        if !self.sink.enabled() {
            return;
        }
        let url = upgrade_url(&self.url);
        let mut request_headers = self.headers.clone();
        for (name, value) in [("Connection", "Upgrade"), ("Upgrade", "websocket")] {
            if !request_headers.iter().any(|(n, v)| n == name && !v.trim().is_empty()) {
                request_headers.retain(|(n, _)| n != name);
                request_headers.push((name.to_owned(), value.to_owned()));
            }
        }
        self.sink
            .record(CaptureEvent::Request(self.log(&url, "GET", &request_headers, &[])));
        let response_headers = crate::openai_compat_http::header_pairs(headers);
        self.sink
            .record(CaptureEvent::ResponseMetadata(status, &response_headers));
        self.sink.record(CaptureEvent::ResponseChunk(body));
    }

    /// `recordAPIWebsocketHandshake` for a newly dialed socket.
    fn handshake(&self, headers: &HeaderMap) {
        if self.sink.enabled() {
            let pairs = crate::openai_compat_http::header_pairs(headers);
            self.sink.record(CaptureEvent::WebsocketHandshake(101, &pairs));
        }
    }

    /// `RecordAPIWebsocketError`.
    fn error(&self, stage: &str, error: &ExecError) {
        if self.sink.enabled() {
            self.sink.record(CaptureEvent::WebsocketError {
                stage,
                error: &String::from_utf8_lossy(&error.body),
            });
        }
    }

    /// `AppendAPIWebsocketResponse`.
    fn response(&self, payload: &[u8]) {
        self.sink.record(CaptureEvent::WebsocketResponse(payload));
    }
}

/// `helps.WebsocketUpgradeRequestURL`: ws to http, wss to https.
fn upgrade_url(raw: &str) -> String {
    let raw = raw.trim();
    match raw.find(':') {
        Some(i) if raw[..i].eq_ignore_ascii_case("ws") => format!("http{}", &raw[i..]),
        Some(i) if raw[..i].eq_ignore_ascii_case("wss") => format!("https{}", &raw[i..]),
        _ => raw.to_owned(),
    }
}

// --- the turn --------------------------------------------------------------------------

/// One turn on the session's socket, as a stream of downstream payloads. Dropping it
/// releases the turn lock and the reader.
struct Turn {
    rx: mpsc::Receiver<Read>,
    conn: Arc<Upstream>,
    session: Arc<Session>,
    _turn: OwnedMutexGuard<()>,
    apply_patch: apply_patch::State,
    restorer: NamespaceRestorer,
    filter: XSearchFilter,
    alias: String,
    items: OutputItems,
    store: Arc<replay::Store>,
    scope: ReplayScope,
    mapper: Option<IdMapper>,
    /// The frame sent upstream, recorded in the transcript.
    frame: Vec<u8>,
    transcript_reset: bool,
    warmup: bool,
    recorded: bool,
    usage: cpa_core::exec::UsageSink,
    /// A completed or done event carried usage (`StreamUsageBuffer.ok`).
    usage_seen: bool,
    capture: Arc<WsCapture>,
    ready: VecDeque<Result<Bytes, ExecError>>,
    done: bool,
}

impl Drop for Turn {
    fn drop(&mut self) {
        self.conn.deactivate();
    }
}

impl Turn {
    /// The validation that ends the upstream attempt before delivery or reuse
    /// (`invalidatePatchAttempt`).
    fn fail_patch(&mut self, events: Vec<Vec<u8>>) {
        let error = apply_patch_error();
        crate::openai_compat_http::publish_failure(&self.usage, &error);
        self.session.invalidate(&self.conn, &error, false);
        self.ready.extend(events.into_iter().map(|e| Ok(Bytes::from(e))));
        self.ready.push_back(Err(error));
        self.done = true;
    }

    fn record(&mut self, completed: &[u8]) {
        if self.recorded {
            return;
        }
        if let Some(mapper) = &self.mapper {
            mapper.state.record(&self.frame, completed, self.transcript_reset);
            self.recorded = true;
        }
    }

    /// The socket failed: a pending apply_patch call fails first.
    fn read_failed(&mut self, error: ExecError) {
        if let Err(finish) = self.apply_patch.finish() {
            let (events, _) = self.apply_patch.bridge.fail(&finish);
            return self.fail_patch(events);
        }
        let error = pool_error(error);
        // Every socket here has a session, whose reader hands a binary message over as a
        // read error: Go logs it at the `read` stage (`unexpected_binary` is its
        // sessionless reader's).
        self.capture.error("read", &error);
        crate::openai_compat_http::publish_failure(&self.usage, &error);
        self.ready.push_back(Err(error));
        self.done = true;
    }

    /// One upstream text message.
    fn message(&mut self, message: &str) {
        let payload = cpa_common::gostr::trim_space(message.as_bytes());
        if payload.is_empty() {
            return;
        }
        self.usage.first_byte();
        self.capture.response(payload);
        if let Some(error) = ws_error(payload) {
            self.capture.error("upstream_error", &error);
            crate::openai_compat_http::publish_failure(&self.usage, &error);
            self.session.invalidate(&self.conn, &error, false);
            self.ready.push_back(Err(error));
            self.done = true;
            return;
        }
        for event in response::normalize_summary_data_events(payload.to_vec()) {
            self.apply_patch.remember_dispatcher_event(&event);
            let mut event = self.restorer.restore(event);
            if !self.alias.is_empty() {
                event = response::restore_web_search_name(event, &self.alias);
            }
            let Some(event) = self.filter.apply(event).filter(|e| !e.is_empty()) else {
                continue;
            };
            let (events, bridge_error) = self.apply_patch.transform(&event);
            if bridge_error.is_some() {
                return self.fail_patch(events);
            }
            for event in events {
                if self.event(event) {
                    self.done = true;
                    // The goroutine's deferred `streamUsage.Publish`.
                    if self.usage_seen {
                        self.usage.publish();
                    }
                    return;
                }
            }
        }
    }

    /// One event for the client; true ends the turn.
    fn event(&mut self, mut event: Vec<u8>) -> bool {
        let kind = text(&gj::get(&event, "type"));
        let patch_terminal =
            self.apply_patch.active() && matches!(kind.as_str(), "response.incomplete" | "response.failed");
        let terminal = matches!(kind.as_str(), "response.completed" | "response.done" | "error") || patch_terminal;
        if self.usage.enabled() {
            // ObserveResponseModel, and StreamUsageBuffer.Observe on completed and done.
            self.usage
                .response_line(Format::Codex, &response::usage_line(&event, &WS_USAGE_EVENTS));
            if WS_USAGE_EVENTS.contains(&kind.as_str()) && response::codex_usage_ok(&event) {
                self.usage_seen = true;
            }
        }
        let mut warmup_completed_event = None;
        match kind.as_str() {
            "response.created" if self.warmup => {
                let completed = warmup_completed(&event);
                self.record(&completed);
                warmup_completed_event = Some(completed);
            }
            "response.output_item.done" => self.items.collect(&event),
            "response.completed" => {
                event = response::normalize_summary_data(self.items.patch(&event));
                replay::cache_completed(&self.store, &self.scope, &event);
                if !self.warmup {
                    self.record(&event);
                }
            }
            "response.done" if !self.warmup => self.record(&event),
            _ => {}
        }
        let mut downstream = ensure_responses_usage_details(&event);
        if let Some(mapper) = &mut self.mapper {
            downstream = mapper.downstream_response(downstream);
        }
        self.ready.push_back(Ok(Bytes::from(downstream)));
        if let Some(completed) = warmup_completed_event {
            let mut completed = ensure_responses_usage_details(&completed);
            if let Some(mapper) = &mut self.mapper {
                completed = mapper.downstream_response(completed);
            }
            self.ready.push_back(Ok(Bytes::from(completed)));
            return true;
        }
        terminal
    }

    async fn next(&mut self) -> Option<Result<Bytes, ExecError>> {
        loop {
            if let Some(item) = self.ready.pop_front() {
                if item.is_err() {
                    self.ready.clear();
                    self.done = true;
                }
                return Some(item);
            }
            if self.done {
                return None;
            }
            match self.rx.recv().await {
                Some(Read::Text(message)) => self.message(&message),
                Some(Read::Failed(error)) => self.read_failed(error),
                // The reader's terminal error did not fit a full queue (Go's
                // `sendTerminalWebsocketRead` waits for room); it is kept on the socket.
                None => {
                    let lost = self.conn.link.lock().expect("link").lost.clone();
                    self.read_failed(
                        lost.unwrap_or_else(|| plain("xai websockets executor: session read channel closed")),
                    );
                }
            }
        }
    }
}

impl XaiExecutor {
    /// `XAIAutoExecutor.ExecuteStream` for a downstream WebSocket turn: the upstream
    /// socket when the credential enables `websockets`, else HTTP (which cannot continue
    /// upstream state).
    pub async fn execute_in_session(
        &self,
        credential: &Credential,
        req: ExecRequest,
        cfg: &Config,
        session: &ExecSession,
    ) -> Result<ExecResponse, ExecError> {
        if websockets_enabled(credential) {
            let cfg = crate::xai::scoped(credential, cfg);
            return self.stream_ws(credential, req, cfg.as_ref(), session).await;
        }
        if session.continuation {
            // Go's auto executor answers before any reporter exists.
            req.usage.discard();
            return Err(ExecError::replay_required());
        }
        self.execute(credential, req, cfg, true).await
    }

    /// Whether this credential keeps upstream state on the session's socket.
    pub fn session_upstream(credential: &Credential) -> bool {
        websockets_enabled(credential)
    }

    pub fn session_closed(&self, id: &str) -> impl std::future::Future<Output = ExecError> + Send + 'static {
        self.ws.closed(id)
    }

    pub fn close_session(&self, id: &str) {
        self.ws.close(id);
    }

    /// `XAIWebsocketsExecutor.ExecuteStream`.
    async fn stream_ws(
        &self,
        credential: &Credential,
        req: ExecRequest,
        cfg: &Config,
        exec_session: &ExecSession,
    ) -> Result<ExecResponse, ExecError> {
        if req.alt.as_deref() == Some(crate::xai::COMPACT_ALT) {
            req.usage.discard();
            return Err(status_err(400, "streaming not supported for /responses/compact"));
        }
        let exec_id = exec_session.id.trim().to_owned();
        let mut state_id = request::execution_session_id(&req);
        if state_id.is_empty() {
            state_id.clone_from(&exec_id);
        }
        let state = self.ws.state(&state_id);
        if crate::xai::input_has_item_type(&req.body, "compaction_trigger") {
            if exec_session.continuation {
                req.usage.discard();
                return Err(ExecError::replay_required());
            }
            let session = self.ws.pool.session(&exec_id);
            let _turn = session.turn.clone().lock_owned().await;
            let mapper = state.map(|s| IdMapper::new(s, &req.body));
            return self.ws_compaction(credential, &req, cfg, mapper).await;
        }

        // The official API (or an explicit base_url): cli-chat-proxy refuses upgrades.
        let (token, mut base) = creds(credential);
        if base.is_empty() {
            base = DEFAULT_API_BASE_URL.to_owned();
        }
        let mut prepared = crate::xai::before_reporter(
            &req.usage,
            request::prepare(&req, cfg, true, Format::Codex, &self.replay, true).await,
        )?;
        let previous = text(&gj::get(&req.body, "previous_response_id")).trim().to_owned();
        if !previous.is_empty() {
            gj::set_str(&mut prepared.body, "previous_response_id", &previous);
        }
        let http_url = format!("{}/responses", base.strip_suffix('/').unwrap_or(&base));
        let target = Target {
            credential: credential.id.clone(),
            url: self.url(ws_url(&http_url)?),
            proxy: crate::proxy::Proxy::effective(credential, cfg),
        };

        let session = self.ws.pool.session(&exec_id);
        let guard = session.turn.clone().lock_owned().await;
        let mut mapper = state.clone().map(|s| IdMapper::new(s, &req.body));
        if let Some(m) = &mut mapper {
            if m.state.target_changed(&target) {
                m.upstream_previous.clear();
            }
            prepared.body = m.upstream_request(std::mem::take(&mut prepared.body));
        }
        // The turn publishes only usage it parsed (no EnsurePublished).
        req.usage.usage_required();
        req.usage.request_for(crate::xai::PROVIDER, &prepared.body);
        let headers = handshake_headers(credential, &token, &prepared.session_id, &req);
        let frame = request_frame(&prepared.body);
        let request_type = text(&gj::get(&req.body, "type"));
        let transcript_reset = text(&gj::get(&frame, "previous_response_id")).trim().is_empty()
            && (request_type.trim() != "response.append" || mapper.as_ref().is_some_and(|m| m.replayed_compacted));
        let warmup = generate_false(&frame);
        let message = String::from_utf8_lossy(&frame).into_owned();
        let capture = Arc::new(WsCapture::new(&req, credential, &target.url, &headers, &frame));
        capture.request();

        let (mut conn, mut handshake) = if exec_session.continuation {
            match session.current().filter(|c| c.target == target) {
                Some(conn) => (conn, None),
                None => return Err(ExecError::replay_required()),
            }
        } else {
            self.ws_ensure(
                &session,
                &target,
                &headers,
                cfg,
                credential,
                state.as_deref(),
                &capture,
                false,
            )
            .await?
        };
        if let Some(handshake) = &handshake {
            capture.handshake(handshake);
        }
        conn.bind(&session, exec_session.lease.as_ref())?;
        req.usage.round_trip_started();
        let mut rx = conn.activate();
        if let Err(error) = conn.send(message.clone()).await {
            let error = pool_error(error);
            capture.error("send", &error);
            // `shouldRetryCodexWebsocketSend`: a request-scoped failure (413) never retries.
            let retry = should_retry_send(&error);
            if exec_session.continuation {
                session.invalidate(&conn, &error, false);
                return Err(if retry { ExecError::replay_required() } else { error });
            }
            session.invalidate(&conn, &error, true);
            if !retry {
                return Err(error);
            }
            let (fresh, fresh_handshake) = self
                .ws_ensure(
                    &session,
                    &target,
                    &headers,
                    cfg,
                    credential,
                    state.as_deref(),
                    &capture,
                    true,
                )
                .await?;
            capture.request();
            if let Some(handshake) = &fresh_handshake {
                capture.handshake(handshake);
            }
            fresh.bind(&session, exec_session.lease.as_ref())?;
            req.usage.round_trip_started();
            rx = fresh.activate();
            if let Err(error) = fresh.send(message).await {
                let error = pool_error(error);
                capture.error("send_retry", &error);
                session.invalidate(&fresh, &error, true);
                return Err(error);
            }
            conn = fresh;
            handshake = fresh_handshake;
        }

        let turn = Turn {
            rx,
            conn,
            session,
            _turn: guard,
            apply_patch: prepared.apply_patch,
            restorer: NamespaceRestorer::new(prepared.namespace_tools),
            filter: XSearchFilter::new(prepared.filter_internal_x_search, prepared.client_declared_tools),
            alias: prepared.web_search_alias,
            items: OutputItems::default(),
            store: self.replay.clone(),
            scope: prepared.replay_scope.clone(),
            mapper,
            frame,
            transcript_reset,
            warmup,
            recorded: false,
            usage: req.usage.clone(),
            usage_seen: false,
            capture,
            ready: VecDeque::new(),
            done: false,
        };
        let stream = futures_util::stream::unfold(turn, |mut turn| async move {
            let item = turn.next().await?;
            Some((item, turn))
        })
        .boxed();
        Ok(ExecResponse {
            status: 200,
            headers: handshake.unwrap_or_default(),
            body: ResponseBody::Stream(prepared.replay_scope.writes.gate(stream)),
        })
    }

    /// `ensureUpstreamConn`: the session's socket for `target`, dialing a new one (and
    /// closing one for another target) when needed. Returns the handshake headers of a
    /// new socket.
    #[allow(clippy::too_many_arguments)]
    async fn ws_ensure(
        &self,
        session: &Arc<Session>,
        target: &Target,
        headers: &HeaderMap,
        cfg: &Config,
        credential: &Credential,
        state: Option<&IdState>,
        capture: &WsCapture,
        retry: bool,
    ) -> Result<(Arc<Upstream>, Option<HeaderMap>), ExecError> {
        if let Some(current) = session.current() {
            if current.target == *target {
                return Ok((current, None));
            }
            // target_changed: another credential, URL or proxy now serves this session.
            let mut slot = session.conn.lock().expect("session conn");
            if slot.as_ref().is_some_and(|c| Arc::ptr_eq(c, &current)) {
                *slot = None;
            }
            drop(slot);
            current.shutdown();
        }
        let client = self.clients.for_credential(credential, cfg);
        let (socket, handshake) = dial(&client, target, headers, capture, retry).await?;
        let (sink, stream) = socket.split();
        let conn = Upstream::new(target.clone(), sink, self.ws.pool.write);
        // Publish before the reader runs, as the Codex executor does.
        let mut reader = conn.reader.lock().expect("reader handle");
        *session.conn.lock().expect("session conn") = Some(conn.clone());
        let task = tokio::spawn(codex_ws::read_loop(
            stream,
            Arc::downgrade(session),
            conn.clone(),
            self.ws.pool.idle,
        ));
        *reader = Some(task.abort_handle());
        drop(reader);
        if let Some(state) = state {
            state.set_target(target);
        }
        Ok((conn, Some(handshake)))
    }
}

/// `dialXAIWebsocket`: a rejected upgrade is `xaiStatusErr(status, body)`.
///
/// Capture: the first dial records a rejected upgrade (any handshake response, including
/// a 101 gorilla refuses) as an HTTP attempt and any other failure as a `dial` error; a
/// retry dial records every failure as `dial_retry`, with gorilla's text for a rejection.
// ponytail: Go returns `xaiStatusErr(101, body)` for a 101 gorilla refuses; this keeps the
// plain handshake error, since a 101 status error has no sensible client response.
async fn dial(
    client: &wreq::Client,
    target: &Target,
    headers: &HeaderMap,
    capture: &WsCapture,
    retry: bool,
) -> Result<(codex_ws::WebSocket, HeaderMap), ExecError> {
    let stage = if retry { "dial_retry" } else { "dial" };
    match dial_once(client, target, headers).await {
        Ok(socket) => Ok(socket),
        Err(Dial::Rejected(rejected)) => {
            let Rejection {
                error,
                status,
                headers,
                body,
                reason,
            } = *rejected;
            if retry {
                capture.error(stage, &plain(reason));
            } else {
                capture.rejection(status, &headers, &body);
            }
            Err(error)
        }
        Err(Dial::Failed(error)) => {
            capture.error(stage, &error);
            Err(error)
        }
    }
}

/// A failed dial: a handshake response gorilla rejects, or any other failure.
enum Dial {
    Rejected(Box<Rejection>),
    Failed(ExecError),
}

/// A handshake response gorilla rejects: the returned error, the response, and gorilla's
/// error text.
struct Rejection {
    error: ExecError,
    status: u16,
    headers: HeaderMap,
    body: Vec<u8>,
    reason: &'static str,
}

const BAD_HANDSHAKE: &str = "websocket: bad handshake";

async fn dial_once(
    client: &wreq::Client,
    target: &Target,
    headers: &HeaderMap,
) -> Result<(codex_ws::WebSocket, HeaderMap), Dial> {
    // Go's xAI dialer is gorilla's with `EnableCompression`, like the Codex one, and the
    // socket joins the same pool type, so both dial through codex_ws's upgrade.
    let mut headers = headers.clone();
    let key = codex_ws::offer_compression(&mut headers);
    let builder = client.websocket(&target.url).headers(headers).accept_key(key.clone());
    let attempt = async {
        let mut res = builder
            .send()
            .await
            .map_err(|e| Dial::Failed(plain(format!("xai websockets executor: dial failed: {e}"))))?;
        let status = res.status().as_u16();
        let handshake = res.headers().clone();
        if status != 101 {
            let inner = std::mem::replace(&mut *res, wreq::Response::from(http::Response::new(Vec::<u8>::new())));
            let mut body = Vec::new();
            let mut stream = inner.bytes_stream();
            while let Some(Ok(chunk)) = stream.next().await {
                body.extend_from_slice(&chunk[..chunk.len().min(MAX_HANDSHAKE_BODY - body.len())]);
                if body.len() >= MAX_HANDSHAKE_BODY {
                    break;
                }
            }
            // Go marks the upstream attempt on a dial error; here the upstream headers do
            // (the server's `upstream_attempted`).
            let mut error = response::status_error(status, &body);
            error.headers = Box::new(handshake.clone());
            return Err(Dial::Rejected(Box::new(Rejection {
                error,
                status,
                headers: handshake,
                body,
                reason: BAD_HANDSHAKE,
            })));
        }
        let socket = codex_ws::upgrade(&mut res, &key, &handshake).await.map_err(|e| {
            let (error, reason) = match e {
                codex_ws::UpgradeError::Handshake => (
                    plain("xai websockets executor: websocket handshake failed"),
                    BAD_HANDSHAKE,
                ),
                codex_ws::UpgradeError::Compression => (
                    plain("xai websockets executor: websocket: invalid compression negotiation"),
                    "websocket: invalid compression negotiation",
                ),
            };
            Dial::Rejected(Box::new(Rejection {
                error,
                status,
                headers: handshake.clone(),
                body: Vec::new(),
                reason,
            }))
        })?;
        Ok((socket, handshake))
    };
    tokio::time::timeout(HANDSHAKE_TIMEOUT, attempt)
        .await
        .unwrap_or_else(|_| Err(Dial::Failed(plain("xai websockets executor: handshake timed out"))))
}

impl XaiExecutor {
    /// `executeCompactionTriggerFromWebsocketContext`: compacts the recorded transcript
    /// (or the turn's own input, or its previous response) over HTTP and keeps only the
    /// compaction item as the transcript.
    async fn ws_compaction(
        &self,
        credential: &Credential,
        req: &ExecRequest,
        cfg: &Config,
        mapper: Option<IdMapper>,
    ) -> Result<ExecResponse, ExecError> {
        // Go's context errors come before executeCompactRequest creates its reporter.
        let Some(mapper) = mapper else {
            req.usage.discard();
            return Err(status_err(400, "xai websocket compaction context is unavailable"));
        };
        let payload = match mapper.state.snapshot() {
            Some(transcript) => compaction_payload(&req.body, &transcript),
            None => {
                let filtered = crate::xai::remove_input_items(req.body.to_vec(), "compaction_trigger");
                let input = gj::get(&filtered, "input");
                if input.is_array() && !input.array().is_empty() {
                    let raw = input.raw().to_vec();
                    compaction_payload(&filtered, &raw)
                } else {
                    let mut previous = mapper.upstream_previous.clone();
                    if previous.is_empty() {
                        previous = text(&gj::get(&req.body, "previous_response_id")).trim().to_owned();
                    }
                    if previous.is_empty() {
                        req.usage.discard();
                        return Err(status_err(400, "xai websocket compaction context is empty"));
                    }
                    let mut out = crate::xai::remove_input_items(req.body.to_vec(), "compaction_trigger");
                    gj::set_str(&mut out, "previous_response_id", &previous);
                    out
                }
            }
        };
        let mut compact = req.clone();
        compact.body = Bytes::from(payload);
        let (prepared, data, mut headers) = self.compact_request(credential, &compact, cfg, true).await?;
        let (response_id, item) = validate_compaction(&data)?;
        // Publish(ParseOpenAIUsage(data)): the compact body compact_request reported.
        compact.usage.publish();
        mapper.state.replace(&[&item]);
        mapper.state.map(&response_id, "");
        headers.insert(
            http::header::CONTENT_TYPE,
            HeaderValue::from_static("text/event-stream"),
        );
        let frames: Vec<Result<Bytes, ExecError>> = compaction_frames(&prepared, &data, SystemTime::now())
            .into_iter()
            .map(|f| Ok(Bytes::from(f)))
            .collect();
        Ok(ExecResponse {
            status: 200,
            headers,
            body: ResponseBody::Stream(futures_util::stream::iter(frames).boxed()),
        })
    }
}

#[cfg(test)]
#[path = "xai_ws_tests.rs"]
mod tests;
