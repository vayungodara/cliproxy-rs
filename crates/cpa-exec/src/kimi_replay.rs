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

use crate::kimi_json::{canonical, join_array, set_raw, set_str, valid};

const TTL: Duration = Duration::from_secs(3600);
const MAX_ENTRIES: usize = 10240;
const EVICT_BATCH: usize = 128;
const MAX_BYTES_PER_ENTRY: usize = 8 << 20;
const MAX_BLOCKS_PER_ENTRY: usize = 512;
const MAX_TOTAL_BYTES: usize = 256 << 20;

#[derive(Clone)]
struct Entry {
    content: Option<Arc<str>>,
    at: Instant,
    generation: u64,
}

#[derive(Default)]
struct Inner {
    entries: HashMap<String, Entry>,
    total: usize,
    next_generation: u64,
}

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
    fn get(&self, key: &str, now: Instant) -> (Option<Arc<str>>, Snapshot) {
        let mut inner = self.lock();
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

    fn replace_if_unchanged(&self, key: &str, snapshot: Snapshot, content: &str) -> bool {
        if !valid_content(content) {
            return false;
        }
        let mut inner = self.lock();
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

fn valid_content(content: &str) -> bool {
    if content.is_empty() || content.len() > MAX_BYTES_PER_ENTRY || !valid(content) {
        return false;
    }
    let root = gjson::parse(content);
    root.kind() == gjson::Kind::Array && {
        let n = root.array().len();
        n > 0 && n <= MAX_BLOCKS_PER_ENTRY
    }
}

/// `kimiThinkingReplayModelFamily`: K3 variants share replay state.
pub(crate) fn model_family(model: &str) -> String {
    let (base, _) = crate::kimi_thinking::parse_suffix(model.trim());
    match crate::kimi::normalize_upstream_model(base).as_str() {
        "k3" | "k3-256k" => "k3".into(),
        other => other.into(),
    }
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

fn claude_code_session(payload: &str, headers: &HeaderMap) -> Option<String> {
    let mut session = header(headers, "X-Claude-Code-Session-Id");
    if session.is_empty() {
        let user = gjson::get(payload, "metadata.user_id");
        let user = user.str();
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
            session = gjson::get(user, "session_id").str().trim().to_owned();
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

fn payload_session(payload: &str) -> Option<String> {
    if payload.is_empty() {
        return None;
    }
    let cache = gjson::get(payload, "prompt_cache_key");
    if !cache.str().trim().is_empty() {
        return Some(format!("prompt-cache:{}", cache.str().trim()));
    }
    let window = gjson::get(payload, "client_metadata.x-codex-window-id");
    if !window.str().trim().is_empty() {
        return Some(format!("window:{}", window.str().trim()));
    }
    let turn = gjson::get(payload, "client_metadata.x-codex-turn-metadata");
    turn_session(turn.str().trim())
}

fn turn_session(turn: &str) -> Option<String> {
    if turn.is_empty() {
        return None;
    }
    let cache = gjson::get(turn, "prompt_cache_key");
    if !cache.str().trim().is_empty() {
        return Some(format!("prompt-cache:{}", cache.str().trim()));
    }
    let window = gjson::get(turn, "window_id");
    (!window.str().trim().is_empty()).then(|| format!("window:{}", window.str().trim()))
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
fn session_key(req: &ExecRequest, payload: &str) -> String {
    let key = claude_code_session(payload, &req.headers)
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
    pub(crate) fn store(&self, content: &str) {
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
        let Ok(text) = std::str::from_utf8(response) else {
            return;
        };
        let content = gjson::get(text, "content");
        if content.kind() == gjson::Kind::Array {
            self.store(content.json());
        }
    }
}

/// `prepareKimiThinkingReplayRequest`: restores cached content into `req.body` when it
/// matches the latest assistant turn.
pub(crate) fn prepare(cache: &Arc<ReplayCache>, req: &mut ExecRequest) -> Scope {
    let payload = std::str::from_utf8(&req.body).unwrap_or_default();
    let family = model_family(&req.model);
    let session = session_key(req, payload);
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
        && let Some(updated) = restore(payload, &content)
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

fn replayable(content: &str) -> bool {
    let root = gjson::parse(content);
    if root.kind() != gjson::Kind::Array {
        return false;
    }
    let (mut signed, mut tool) = (false, false);
    for part in root.array() {
        match part.get("type").str().trim() {
            "thinking" if !part.get("signature").str().trim().is_empty() => signed = true,
            "tool_use" if !part.get("id").str().trim().is_empty() => tool = true,
            _ => {}
        }
    }
    signed && tool
}

fn has_thinking(content: &gjson::Value<'_>) -> bool {
    content.kind() == gjson::Kind::Array
        && content
            .array()
            .iter()
            .any(|p| matches!(p.get("type").str().trim(), "thinking" | "redacted_thinking"))
}

/// Canonical non-thinking parts; `None` unless the content is an array with a tool call.
fn non_thinking_parts(content: &gjson::Value<'_>) -> Option<Vec<String>> {
    if content.kind() != gjson::Kind::Array {
        return None;
    }
    let mut parts = Vec::new();
    let mut tool = false;
    for part in content.array() {
        match part.get("type").str().trim() {
            "thinking" | "redacted_thinking" => continue,
            "tool_use" => {
                if part.get("id").str().trim().is_empty() {
                    return None;
                }
                tool = true;
            }
            _ => {}
        }
        parts.push(canonical(part.json())?);
    }
    tool.then_some(parts)
}

/// `restoreKimiThinkingReplayContent`.
fn restore(body: &str, cached: &str) -> Option<String> {
    let cached_value = gjson::parse(cached);
    let cached_parts = non_thinking_parts(&cached_value)?;
    let messages = gjson::get(body, "messages");
    if messages.kind() != gjson::Kind::Array {
        return None;
    }
    let items = messages.array();
    for (index, message) in items.iter().enumerate().rev() {
        if !message.get("role").str().trim().eq_ignore_ascii_case("assistant") {
            continue;
        }
        let current = message.get("content");
        if canonical(current.json()).is_some_and(|c| Some(c) == canonical(cached)) {
            return None;
        }
        if has_thinking(&current) {
            continue;
        }
        if non_thinking_parts(&current).is_none_or(|parts| parts != cached_parts) {
            continue;
        }
        return set_raw(body, &format!("messages.{index}.content"), cached);
    }
    None
}

#[derive(Default)]
struct Block {
    raw: String,
    text: Option<String>,
    thinking: Option<String>,
    signature: Option<String>,
    input: Option<String>,
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
            let Ok(payload) = std::str::from_utf8(payload) else {
                self.abandon();
                continue;
            };
            if !valid(payload) {
                self.abandon();
                continue;
            }
            let root = gjson::parse(payload);
            match root.get("type").str() {
                "message_start" => self.observed = true,
                "content_block_start" if !self.abandoned => self.start(&root),
                "content_block_delta" if !self.abandoned => self.delta(&root),
                "content_block_stop" if !self.abandoned => self.stop(root.get("index").i64()),
                "message_stop" => self.complete = true,
                "error" => {
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

    fn start(&mut self, root: &gjson::Value<'_>) {
        let index = root.get("index").i64();
        let block = root.get("content_block");
        if block.kind() != gjson::Kind::Object
            || self.blocks.len() >= MAX_BLOCKS_PER_ENTRY
            || self.blocks.contains_key(&index)
        {
            self.abandon();
            return;
        }
        if !self.reserve(block.json().len()) {
            return;
        }
        self.blocks.insert(
            index,
            Block {
                raw: block.json().to_owned(),
                ..Block::default()
            },
        );
    }

    fn delta(&mut self, root: &gjson::Value<'_>) {
        let index = root.get("index").i64();
        if !self.blocks.contains_key(&index) {
            self.abandon();
            return;
        }
        let delta = root.get("delta");
        let (field, value) = match delta.get("type").str() {
            "text_delta" => ("text", delta.get("text").str().to_owned()),
            "thinking_delta" => ("thinking", delta.get("thinking").str().to_owned()),
            "signature_delta" => ("signature", delta.get("signature").str().to_owned()),
            "input_json_delta" => ("input", delta.get("partial_json").str().to_owned()),
            _ => {
                self.abandon();
                return;
            }
        };
        if field == "input" {
            if self.reserve(value.len()) {
                let block = self.blocks.get_mut(&index).expect("checked");
                block.input.get_or_insert_with(String::new).push_str(&value);
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
            let initial = gjson::get(&self.blocks[&index].raw, field).str().to_owned();
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
            slot.as_mut().expect("initialized").push_str(&value);
        }
    }

    fn stop(&mut self, index: i64) {
        let Some(block) = self.blocks.get_mut(&index) else {
            self.abandon();
            return;
        };
        if block.input.as_ref().is_some_and(|input| !valid(input)) {
            self.abandon();
            return;
        }
        block.finished = true;
    }

    fn content(&mut self) -> Option<String> {
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
                    raw = set_str(&raw, path, value)?;
                }
            }
            if let Some(input) = &block.input {
                raw = set_raw(&raw, "input", input)?;
            }
            parts.push(raw);
        }
        let content = join_array(&parts);
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
        let restored = restore(body, CACHED).unwrap();
        assert_eq!(gjson::get(&restored, "messages.1.content").json(), CACHED);
        // Already carrying thinking: left alone.
        let with_thinking = set_raw(body, "messages.1.content", CACHED).unwrap();
        assert!(restore(&with_thinking, CACHED).is_none());
        // Different tool input: not the same turn.
        let other = body.replace(r#""path":"a""#, r#""path":"b""#);
        assert!(restore(&other, CACHED).is_none());
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
            r#"[{"type":"thinking","thinking":"plan","signature":"sig-1"},{"type":"tool_use","id":"toolu_1","name":"read","input":{"path":"a"}}]"#
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
        assert!(cache.replace_if_unchanged("k", second, CACHED));
        assert!(!cache.replace_if_unchanged("k", first, CACHED), "stale snapshot loses");
        let (content, third) = cache.get("k", now);
        assert_eq!(content.as_deref(), Some(CACHED));
        assert!(cache.delete_if_unchanged("k", third));
        assert!(cache.get("k", now).0.is_none());
        assert!(cache.get("k", now + TTL + Duration::from_secs(1)).0.is_none());
    }
}
