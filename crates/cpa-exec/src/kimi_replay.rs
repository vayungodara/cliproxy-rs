//! Kimi native reasoning replay for Claude-format requests (kimi_thinking_replay.go,
//! internal/cache/kimi_thinking_replay_cache.go).
//!
//! Claude Code drops signed `thinking` blocks from history; Kimi rejects tool turns
//! without them. After a complete response with signed thinking and a tool call, the
//! assistant content is cached per (model family, session); the next request restores it
//! into the matching assistant turn. Writes are conditional on the generation read, so a
//! slower request never overwrites newer content.
//!
//! ponytail: in-process cache only; Go's Home KV (cpa:kimi:*) backend is not ported.

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use bytes::Bytes;
use cpa_core::exec::{ExecError, ExecRequest, ExecStream};
use futures_util::StreamExt;
use http::HeaderMap;
use sha2::{Digest, Sha256};

use cpa_common::gostr::GoStr;
use cpa_common::json::{self as gj, Res, canonical};

use crate::meta_codex::go_trim_space;

const TTL: Duration = Duration::from_secs(3600);
const MAX_ENTRIES: usize = 10240;
const EVICT_BATCH: usize = 128;
const MAX_BYTES_PER_ENTRY: usize = 8 << 20;
const MAX_BLOCKS_PER_ENTRY: usize = 512;
const MAX_TOTAL_BYTES: usize = 256 << 20;

#[derive(Clone)]
struct Entry {
    content: Option<Arc<[u8]>>,
    at: Instant,
    generation: u64,
}

#[derive(Default)]
struct Inner {
    entries: HashMap<String, Entry>,
    total: usize,
    next_generation: u64,
    last_purge: Option<Instant>,
}

/// Go's cache cleanup ticker interval (signature_cache.go startCacheCleanup).
const PURGE_INTERVAL: Duration = Duration::from_secs(600);

/// The replay cache. One per Kimi executor (Go keeps one per process).
#[derive(Default)]
pub(crate) struct ReplayCache {
    inner: Mutex<Inner>,
}

/// The cache state one request read: replace/delete only if it is still current.
#[derive(Debug, Clone, Copy)]
struct Snapshot {
    generation: u64,
}

impl Inner {
    fn generation(&mut self) -> u64 {
        self.next_generation += 1;
        self.next_generation
    }

    /// Drops expired entries at most once per cleanup interval, like Go's ticker, so a
    /// request whose snapshot outlived the TTL cannot write into a purged key.
    fn purge(&mut self, now: Instant) {
        let last = *self.last_purge.get_or_insert(now);
        if now.duration_since(last) < PURGE_INTERVAL {
            return;
        }
        self.last_purge = Some(now);
        let total = &mut self.total;
        self.entries.retain(|_, e| {
            let keep = now.duration_since(e.at) <= TTL;
            if !keep {
                *total -= e.content.as_ref().map_or(0, |c| c.len());
            }
            keep
        });
    }

    fn enforce_limits(&mut self) {
        while self.entries.len() > MAX_ENTRIES || self.total > MAX_TOTAL_BYTES {
            if self.entries.is_empty() {
                self.total = 0;
                return;
            }
            let mut oldest: Vec<(Instant, String)> = self.entries.iter().map(|(k, e)| (e.at, k.clone())).collect();
            oldest.sort();
            for (_, key) in oldest.into_iter().take(EVICT_BATCH) {
                if let Some(entry) = self.entries.remove(&key) {
                    self.total -= entry.content.map_or(0, |c| c.len());
                }
            }
        }
    }
}

impl ReplayCache {
    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Reads the entry, reserving a tombstone when absent so a later write can be
    /// conditional on this read (GetKimiThinkingReplayWithSnapshotRequired).
    fn get(&self, key: &str, now: Instant) -> (Option<Arc<[u8]>>, Snapshot) {
        let mut inner = self.lock();
        inner.purge(now);
        let expired = inner.entries.get(key).is_some_and(|e| now.duration_since(e.at) > TTL);
        if expired && let Some(old) = inner.entries.remove(key) {
            inner.total -= old.content.map_or(0, |c| c.len());
        }
        if !inner.entries.contains_key(key) {
            let generation = inner.generation();
            inner.entries.insert(
                key.to_owned(),
                Entry {
                    content: None,
                    at: now,
                    generation,
                },
            );
            inner.enforce_limits();
        }
        let Some(entry) = inner.entries.get_mut(key) else {
            return (None, Snapshot { generation: 0 });
        };
        entry.at = now;
        (
            entry.content.clone(),
            Snapshot {
                generation: entry.generation,
            },
        )
    }

    fn replace_if_unchanged(&self, key: &str, snapshot: Snapshot, content: &[u8]) -> bool {
        self.replace_at(key, snapshot, content, Instant::now())
    }

    fn replace_at(&self, key: &str, snapshot: Snapshot, content: &[u8], now: Instant) -> bool {
        if !valid_content(content) {
            return false;
        }
        let mut inner = self.lock();
        inner.purge(now);
        if inner
            .entries
            .get(key)
            .is_none_or(|e| e.generation != snapshot.generation)
        {
            return false;
        }
        let generation = inner.generation();
        let previous = inner.entries.insert(
            key.to_owned(),
            Entry {
                content: Some(Arc::from(content)),
                at: Instant::now(),
                generation,
            },
        );
        inner.total -= previous.and_then(|e| e.content).map_or(0, |c| c.len());
        inner.total += content.len();
        inner.enforce_limits();
        true
    }

    fn delete_if_unchanged(&self, key: &str, snapshot: Snapshot) -> bool {
        let mut inner = self.lock();
        inner.purge(Instant::now());
        if inner
            .entries
            .get(key)
            .is_none_or(|e| e.generation != snapshot.generation)
        {
            return false;
        }
        let generation = inner.generation();
        let previous = inner.entries.insert(
            key.to_owned(),
            Entry {
                content: None,
                at: Instant::now(),
                generation,
            },
        );
        inner.total -= previous.and_then(|e| e.content).map_or(0, |c| c.len());
        true
    }
}

fn valid_content(content: &[u8]) -> bool {
    if content.is_empty() || content.len() > MAX_BYTES_PER_ENTRY || !gj::valid(content) {
        return false;
    }
    let root = gj::parse(content);
    root.is_array() && {
        let n = root.array().len();
        n > 0 && n <= MAX_BLOCKS_PER_ENTRY
    }
}

/// `kimiThinkingReplayModelFamily`: K3 variants share replay state.
pub(crate) fn model_family(model: &str) -> String {
    let base = cpa_common::thinking::parse_suffix(model.trim()).model_name;
    match crate::kimi::normalize_upstream_model(&base).as_str() {
        "k3" | "k3-256k" => "k3".into(),
        other => other.into(),
    }
}

/// `strings.TrimSpace(r.String())`, decoded for use in a session key.
fn text(r: &Res<'_>) -> String {
    String::from_utf8_lossy(go_trim_space(&r.bytes())).into_owned()
}

fn header(headers: &HeaderMap, name: &str) -> String {
    headers
        .get_all(name)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .map(str::trim)
        .find(|v| !v.is_empty())
        .unwrap_or_default()
        .to_owned()
}

fn claude_code_session(payload: &[u8], headers: &HeaderMap) -> Option<String> {
    let mut session = header(headers, "X-Claude-Code-Session-Id");
    if session.is_empty() {
        let user = gj::get(payload, "metadata.user_id").str().into_owned();
        if let Some(pos) = user.rfind("_session_") {
            let tail = &user[pos + "_session_".len()..];
            if !tail.is_empty()
                && tail
                    .bytes()
                    .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase() || b == b'-')
            {
                session = tail.to_owned();
            }
        }
        if session.is_empty() && user.starts_with('{') {
            session = text(&gj::get(user.as_bytes(), "session_id"));
        }
    }
    if session.is_empty() {
        return None;
    }
    let mut agent = header(headers, "X-Claude-Code-Agent-Id");
    if agent.is_empty() {
        agent = "main".into();
    }
    Some(format!("claude:{session}:agent:{agent}"))
}

fn payload_session(payload: &[u8]) -> Option<String> {
    if payload.is_empty() {
        return None;
    }
    let cache = text(&gj::get(payload, "prompt_cache_key"));
    if !cache.is_empty() {
        return Some(format!("prompt-cache:{cache}"));
    }
    let window = text(&gj::get(payload, "client_metadata.x-codex-window-id"));
    if !window.is_empty() {
        return Some(format!("window:{window}"));
    }
    turn_session(&text(&gj::get(payload, "client_metadata.x-codex-turn-metadata")))
}

fn turn_session(turn: &str) -> Option<String> {
    if turn.is_empty() {
        return None;
    }
    let cache = text(&gj::get(turn.as_bytes(), "prompt_cache_key"));
    if !cache.is_empty() {
        return Some(format!("prompt-cache:{cache}"));
    }
    let window = text(&gj::get(turn.as_bytes(), "window_id"));
    (!window.is_empty()).then(|| format!("window:{window}"))
}

fn header_session(headers: &HeaderMap) -> Option<String> {
    if let Some(key) = turn_session(&header(headers, "X-Codex-Turn-Metadata")) {
        return Some(key);
    }
    let window = header(headers, "X-Codex-Window-Id");
    if !window.is_empty() {
        return Some(format!("window:{window}"));
    }
    let session = header(headers, "session_id");
    if !session.is_empty() {
        return Some(format!("session-id:{session}"));
    }
    let session = header(headers, "session-id");
    if !session.is_empty() {
        return Some(format!("session-id:{session}"));
    }
    let conversation = header(headers, "conversation_id");
    (!conversation.is_empty()).then(|| format!("conversation_id:{conversation}"))
}

/// `codexReasoningReplaySessionKey` for a Claude-format request, isolated per caller key
/// (`xaiReasoningReplayIsolateSessionKey`). Empty when no session or no client key.
fn session_key(req: &ExecRequest, payload: &[u8]) -> String {
    let key = claude_code_session(payload, &req.headers)
        .or_else(|| {
            let execution = req.execution_session.as_deref().map(str::trim).unwrap_or_default();
            (!execution.is_empty()).then(|| format!("execution:{execution}"))
        })
        .or_else(|| payload_session(payload))
        .or_else(|| header_session(&req.headers))
        .unwrap_or_default();
    let key = key.trim();
    if key.is_empty() {
        return String::new();
    }
    if key.starts_with("execution:") {
        return key.to_owned();
    }
    let api_key = req.caller.principal.trim();
    if api_key.is_empty() {
        return String::new();
    }
    let digest = Sha256::digest(api_key.as_bytes());
    let prefix: String = digest[..8].iter().map(|b| format!("{b:02x}")).collect();
    format!("caller:{prefix}:{key}")
}

/// One request's replay scope.
pub(crate) struct Scope {
    cache: Arc<ReplayCache>,
    key: String,
    snapshot: Option<Snapshot>,
    pub(crate) applied: bool,
}

impl Scope {
    fn ready(&self) -> bool {
        !self.key.is_empty() && self.snapshot.is_some()
    }

    /// Caches complete replayable content, or clears stale content otherwise.
    pub(crate) fn store(&self, content: &[u8]) {
        let Some(snapshot) = self.snapshot.filter(|_| self.ready()) else {
            return;
        };
        if replayable(content) {
            self.cache.replace_if_unchanged(&self.key, snapshot, content);
        } else {
            self.cache.delete_if_unchanged(&self.key, snapshot);
        }
    }

    pub(crate) fn clear(&self) {
        if let Some(snapshot) = self.snapshot.filter(|_| self.ready()) {
            self.cache.delete_if_unchanged(&self.key, snapshot);
        }
    }

    /// Caches the `content` array of a buffered Claude response.
    pub(crate) fn store_response(&self, response: &[u8]) {
        let content = gj::get(response, "content");
        if content.is_array() {
            self.store(content.raw());
        }
    }
}

/// `prepareKimiThinkingReplayRequest`: restores cached content into `req.body` when it
/// matches the latest assistant turn.
pub(crate) fn prepare(cache: &Arc<ReplayCache>, req: &mut ExecRequest) -> Scope {
    let family = model_family(&req.model);
    let session = session_key(req, &req.body);
    let key = if family.trim().is_empty() || session.is_empty() {
        String::new()
    } else {
        format!("kimi-thinking-replay\0{}\0{}", family.trim(), session)
    };
    let mut scope = Scope {
        cache: cache.clone(),
        key,
        snapshot: None,
        applied: false,
    };
    if scope.key.is_empty() {
        return scope;
    }
    let (content, snapshot) = cache.get(&scope.key, Instant::now());
    scope.snapshot = Some(snapshot);
    if let Some(content) = content
        && let Some(updated) = restore(&req.body, &content)
    {
        req.body = Bytes::from(updated);
        scope.applied = true;
    }
    scope
}

/// Go clears applied replay only for request rejections (status 400 and 422).
pub(crate) fn clears_after(error: &ExecError) -> bool {
    matches!(error.status, 400 | 422)
}

/// `strings.TrimSpace(part.Get(path).String())`.
fn field(part: &Res<'_>, path: &str) -> Vec<u8> {
    go_trim_space(&part.get(path).bytes()).to_vec()
}

fn replayable(content: &[u8]) -> bool {
    let root = gj::parse(content);
    if !root.is_array() {
        return false;
    }
    let (mut signed, mut tool) = (false, false);
    for part in root.array() {
        match field(&part, "type").as_slice() {
            b"thinking" if !field(&part, "signature").is_empty() => signed = true,
            b"tool_use" if !field(&part, "id").is_empty() => tool = true,
            _ => {}
        }
    }
    signed && tool
}

/// `kimiContentHasThinking`.
fn has_thinking(content: &Res<'_>) -> bool {
    content.is_array()
        && content
            .array()
            .iter()
            .any(|p| matches!(field(p, "type").as_slice(), b"thinking" | b"redacted_thinking"))
}

/// `kimiNonThinkingContentParts`: canonical non-thinking parts; `None` unless the content
/// is an array with a tool call.
fn non_thinking_parts(content: &Res<'_>) -> Option<Vec<Vec<u8>>> {
    if !content.is_array() {
        return None;
    }
    let mut parts = Vec::new();
    let mut tool = false;
    for part in content.array() {
        match field(&part, "type").as_slice() {
            b"thinking" | b"redacted_thinking" => continue,
            b"tool_use" => {
                if field(&part, "id").is_empty() {
                    return None;
                }
                tool = true;
            }
            _ => {}
        }
        parts.push(canonical(part.raw())?);
    }
    tool.then_some(parts)
}

/// `restoreKimiThinkingReplayContent`.
fn restore(body: &[u8], cached: &[u8]) -> Option<Vec<u8>> {
    let cached_parts = non_thinking_parts(&gj::parse(cached))?;
    let messages = gj::get(body, "messages");
    if !messages.is_array() {
        return None;
    }
    let items = messages.array();
    for (index, message) in items.iter().enumerate().rev() {
        let role = String::from_utf8_lossy(&field(message, "role")).into_owned();
        if !role.go_eq_fold("assistant") {
            continue;
        }
        let current = message.get("content");
        // kimiJSONEqual: both sides must canonicalize.
        if let (Some(left), Some(right)) = (canonical(current.raw()), canonical(cached))
            && left == right
        {
            return None;
        }
        if has_thinking(&current) {
            continue;
        }
        if non_thinking_parts(&current).is_none_or(|parts| parts != cached_parts) {
            continue;
        }
        return gj::try_set_raw(body, &format!("messages.{index}.content"), cached).ok();
    }
    None
}

#[derive(Default)]
struct Block {
    raw: Vec<u8>,
    text: Option<Vec<u8>>,
    thinking: Option<Vec<u8>>,
    signature: Option<Vec<u8>>,
    input: Option<Vec<u8>>,
    finished: bool,
}

/// Rebuilds the complete assistant content from a Claude SSE stream.
#[derive(Default)]
struct Accumulator {
    blocks: BTreeMap<i64, Block>,
    observed: bool,
    complete: bool,
    upstream_error: bool,
    abandoned: bool,
    used: usize,
}

impl Accumulator {
    fn observe(&mut self, chunk: &[u8]) {
        for line in chunk.split(|b| *b == b'\n') {
            let line = line.trim_ascii();
            let Some(payload) = line.strip_prefix(b"data:") else {
                continue;
            };
            let payload = payload.trim_ascii();
            if payload.is_empty() || payload == b"[DONE]" {
                continue;
            }
            if !gj::valid(payload) {
                self.abandon();
                continue;
            }
            let root = gj::parse(payload);
            match &*root.get("type").bytes() {
                b"message_start" => self.observed = true,
                b"content_block_start" if !self.abandoned => self.start(&root),
                b"content_block_delta" if !self.abandoned => self.delta(&root),
                b"content_block_stop" if !self.abandoned => self.stop(root.get("index").int()),
                b"message_stop" => self.complete = true,
                b"error" => {
                    self.upstream_error = true;
                    self.abandon();
                }
                _ => {}
            }
        }
    }

    fn reserve(&mut self, n: usize) -> bool {
        if self.used > MAX_BYTES_PER_ENTRY.saturating_sub(n) {
            self.abandon();
            return false;
        }
        self.used += n;
        true
    }

    fn abandon(&mut self) {
        self.abandoned = true;
        self.blocks.clear();
        self.used = 0;
    }

    fn start(&mut self, root: &Res<'_>) {
        let index = root.get("index").int();
        let block = root.get("content_block");
        if !block.is_object() || self.blocks.len() >= MAX_BLOCKS_PER_ENTRY || self.blocks.contains_key(&index) {
            self.abandon();
            return;
        }
        if !self.reserve(block.raw().len()) {
            return;
        }
        self.blocks.insert(
            index,
            Block {
                raw: block.raw().to_vec(),
                ..Block::default()
            },
        );
    }

    fn delta(&mut self, root: &Res<'_>) {
        let index = root.get("index").int();
        if !self.blocks.contains_key(&index) {
            self.abandon();
            return;
        }
        let delta = root.get("delta");
        let value = |path: &str| delta.get(path).bytes().into_owned();
        let (field, value) = match &*delta.get("type").bytes() {
            b"text_delta" => ("text", value("text")),
            b"thinking_delta" => ("thinking", value("thinking")),
            b"signature_delta" => ("signature", value("signature")),
            b"input_json_delta" => ("input", value("partial_json")),
            _ => {
                self.abandon();
                return;
            }
        };
        if field == "input" {
            if self.reserve(value.len()) {
                let block = self.blocks.get_mut(&index).expect("checked");
                block.input.get_or_insert_with(Vec::new).extend_from_slice(&value);
            }
            return;
        }
        let initialized = {
            let block = &self.blocks[&index];
            match field {
                "text" => block.text.is_some(),
                "thinking" => block.thinking.is_some(),
                _ => block.signature.is_some(),
            }
        };
        if !initialized {
            let initial = gj::get(&self.blocks[&index].raw, field).bytes().into_owned();
            if !self.reserve(initial.len()) {
                return;
            }
            let block = self.blocks.get_mut(&index).expect("checked");
            let slot = match field {
                "text" => &mut block.text,
                "thinking" => &mut block.thinking,
                _ => &mut block.signature,
            };
            *slot = Some(initial);
        }
        if self.reserve(value.len()) {
            let block = self.blocks.get_mut(&index).expect("checked");
            let slot = match field {
                "text" => &mut block.text,
                "thinking" => &mut block.thinking,
                _ => &mut block.signature,
            };
            slot.as_mut().expect("initialized").extend_from_slice(&value);
        }
    }

    fn stop(&mut self, index: i64) {
        let Some(block) = self.blocks.get_mut(&index) else {
            self.abandon();
            return;
        };
        if block.input.as_ref().is_some_and(|input| !gj::valid(input)) {
            self.abandon();
            return;
        }
        block.finished = true;
    }

    fn content(&mut self) -> Option<Vec<u8>> {
        if !self.observed || !self.complete || self.upstream_error || self.abandoned {
            return None;
        }
        let mut parts = Vec::with_capacity(self.blocks.len());
        for block in self.blocks.values() {
            if !block.finished {
                self.abandon();
                return None;
            }
            let mut raw = block.raw.clone();
            for (path, value) in [
                ("text", &block.text),
                ("thinking", &block.thinking),
                ("signature", &block.signature),
            ] {
                if let Some(value) = value {
                    raw = gj::try_set_str(&raw, path, value).ok()?;
                }
            }
            if let Some(input) = &block.input {
                raw = gj::try_set_raw(&raw, "input", input).ok()?;
            }
            parts.push(raw);
        }
        let content = gj::join(&parts);
        if content.len() > MAX_BYTES_PER_ENTRY {
            self.abandon();
            return None;
        }
        Some(content)
    }
}

/// `wrapKimiThinkingReplayStream`: forwards every event and caches the complete content
/// once the stream ends cleanly.
pub(crate) fn wrap_stream(stream: ExecStream, scope: Scope) -> ExecStream {
    struct State {
        stream: ExecStream,
        scope: Scope,
        acc: Accumulator,
        failed: bool,
        done: bool,
    }
    futures_util::stream::unfold(
        State {
            stream,
            scope,
            acc: Accumulator::default(),
            failed: false,
            done: false,
        },
        |mut st| async move {
            if st.done {
                return None;
            }
            match st.stream.next().await {
                Some(Ok(event)) => {
                    st.acc.observe(&event);
                    Some((Ok(event), st))
                }
                Some(Err(error)) => {
                    st.failed = true;
                    st.done = true;
                    Some((Err(error), st))
                }
                None => {
                    st.done = true;
                    if !st.failed {
                        if let Some(content) = st.acc.content() {
                            st.scope.store(&content);
                        } else if st.acc.upstream_error && st.scope.applied {
                            st.scope.clear();
                        }
                    }
                    None
                }
            }
        },
    )
    .boxed()
}

#[cfg(test)]
mod tests {
    use super::*;

    const CACHED: &str = r#"[{"type":"thinking","thinking":"plan","signature":"sig-1"},{"type":"text","text":"Calling."},{"type":"tool_use","id":"toolu_1","name":"read","input":{"path":"a"}}]"#;

    #[test]
    fn restore_replaces_only_the_matching_unsigned_assistant_turn() {
        // Expected shapes from kimi_thinking_replay_test.go.
        let body = r#"{"messages":[{"role":"user","content":"hi"},{"role":"assistant","content":[{"type":"text","text":"Calling."},{"type":"tool_use","id":"toolu_1","name":"read","input":{"path":"a"}}]},{"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_1","content":"x"}]}]}"#;
        let restored = restore(body.as_bytes(), CACHED.as_bytes()).unwrap();
        assert_eq!(gj::get(&restored, "messages.1.content").raw(), CACHED.as_bytes());
        // Already carrying thinking: left alone.
        let with_thinking = gj::try_set_raw(body.as_bytes(), "messages.1.content", CACHED).unwrap();
        assert!(restore(&with_thinking, CACHED.as_bytes()).is_none());
        // Different tool input: not the same turn.
        let other = body.replace(r#""path":"a""#, r#""path":"b""#);
        assert!(restore(other.as_bytes(), CACHED.as_bytes()).is_none());
        // Go EqualFold on the role: "ASSISTANT" still matches.
        let upper = body.replace(r#""role":"assistant""#, r#""role":" ASSISTANT ""#);
        assert!(restore(upper.as_bytes(), CACHED.as_bytes()).is_some());
    }

    #[test]
    fn execution_session_follows_claude_code_session_and_skips_caller_isolation() {
        // codexReasoningReplaySessionKey: Claude Code session, then the execution session
        // (not isolated per caller key), then payload and header keys.
        let fx = serde_json::json!({"request": {"body": "{}", "source": "claude", "model": "kimi-k3"}});
        let mut req = crate::kimi_fixture::request(&fx, "");
        req.headers.insert("session_id", "s1".parse().unwrap());
        assert_eq!(session_key(&req, b"{}"), "", "header sessions need a caller key");
        req.execution_session = Some(" e1 ".into());
        assert_eq!(session_key(&req, b"{}"), "execution:e1");
        // A Claude Code session wins over the execution session and is caller-isolated.
        req.headers.insert("X-Claude-Code-Session-Id", "cc".parse().unwrap());
        assert_eq!(session_key(&req, b"{}"), "");
        req.caller.principal = "k".into();
        let key = session_key(&req, b"{}");
        assert!(
            key.starts_with("caller:") && key.ends_with(":claude:cc:agent:main"),
            "{key}"
        );
    }

    #[test]
    fn family_shares_k3_variants_only() {
        assert_eq!(model_family("kimi-k3"), "k3");
        assert_eq!(model_family("kimi-k3-256k(high)"), "k3");
        assert_eq!(model_family("kimi-k2.8"), "kimi-for-coding");
        assert_eq!(model_family("kimi-k2.5"), "k2.5");
    }

    #[test]
    fn accumulator_rebuilds_blocks_and_rejects_unknown_deltas() {
        let events = [
            r#"data: {"type":"message_start","message":{}}"#,
            r#"data: {"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":"","signature":""}}"#,
            r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"pl"}}"#,
            r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"an"}}"#,
            r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"sig-1"}}"#,
            r#"data: {"type":"content_block_stop","index":0}"#,
            r#"data: {"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"toolu_1","name":"read","input":{}}}"#,
            r#"data: {"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"{\"path\":"}}"#,
            r#"data: {"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"\"a\"}"}}"#,
            r#"data: {"type":"content_block_stop","index":1}"#,
            r#"data: {"type":"message_stop"}"#,
        ];
        let mut acc = Accumulator::default();
        for event in events {
            acc.observe(format!("event: x\n{event}\n\n").as_bytes());
        }
        assert_eq!(
            acc.content().unwrap(),
            br#"[{"type":"thinking","thinking":"plan","signature":"sig-1"},{"type":"tool_use","id":"toolu_1","name":"read","input":{"path":"a"}}]"#
        );
        let mut acc = Accumulator::default();
        for event in &events[..3] {
            acc.observe(event.as_bytes());
        }
        acc.observe(br#"data: {"type":"content_block_delta","index":0,"delta":{"type":"citations_delta"}}"#);
        for event in &events[3..] {
            acc.observe(event.as_bytes());
        }
        assert!(acc.content().is_none(), "unknown delta abandons the stream");
    }

    #[test]
    fn conditional_writes_keep_newer_content() {
        let cache = ReplayCache::default();
        let now = Instant::now();
        let (_, first) = cache.get("k", now);
        let (_, second) = cache.get("k", now);
        assert!(cache.replace_if_unchanged("k", second, CACHED.as_bytes()));
        assert!(
            !cache.replace_if_unchanged("k", first, CACHED.as_bytes()),
            "stale snapshot loses"
        );
        let (content, third) = cache.get("k", now);
        assert_eq!(content.as_deref(), Some(CACHED.as_bytes()));
        assert!(cache.delete_if_unchanged("k", third));
        assert!(cache.get("k", now).0.is_none());
        assert!(cache.get("k", now + TTL + Duration::from_secs(1)).0.is_none());

        // A snapshot that outlives the TTL cannot write after the cleanup purged its key.
        let cache = ReplayCache::default();
        let (_, stale) = cache.get("k", now);
        let late = now + TTL + PURGE_INTERVAL + Duration::from_secs(1);
        assert!(!cache.replace_at("k", stale, CACHED.as_bytes(), late));
    }
}
