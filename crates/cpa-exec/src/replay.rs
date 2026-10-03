//! Signed-thinking replay shared by Kimi and Claude API-key compat models
//! (kimi_thinking_replay.go helpers that claude_thinking_replay.go reuses).
//!
//! Claude Code drops signed `thinking` blocks from history; some upstreams reject tool
//! turns without them. A provider caches the complete assistant content of a response
//! (signed thinking plus a tool call) per (model family, session) and restores it into
//! the matching assistant turn of the next request. This module has the provider-neutral
//! parts: the session key, the replayable check, turn restoration, the SSE accumulator and
//! the stream wrapper. Each provider owns its cache and family (kimi_replay for Kimi; Go's
//! Claude cache keeps up to 64 turns per session and restores each in order).

use std::collections::BTreeMap;

use cpa_common::gostr::GoStr;
use cpa_common::json::{self as gj, Res, canonical};
use cpa_core::exec::{ExecRequest, ExecStream};
use futures_util::StreamExt;
use http::HeaderMap;
use sha2::{Digest, Sha256};

use crate::meta_codex::go_trim_space;

/// Largest content one turn may cache, and the accumulator's budget.
pub(crate) const MAX_BYTES_PER_ENTRY: usize = 8 << 20;
/// Most content blocks one turn may hold.
pub(crate) const MAX_BLOCKS_PER_ENTRY: usize = 512;

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

pub(crate) fn claude_code_session(payload: &[u8], headers: &HeaderMap) -> Option<String> {
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

pub(crate) fn payload_session(payload: &[u8]) -> Option<String> {
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

pub(crate) fn header_session(headers: &HeaderMap) -> Option<String> {
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
pub(crate) fn session_key(req: &ExecRequest, payload: &[u8]) -> String {
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

/// `strings.TrimSpace(part.Get(path).String())`.
fn field(part: &Res<'_>, path: &str) -> Vec<u8> {
    go_trim_space(&part.get(path).bytes()).to_vec()
}

/// `kimiThinkingReplayContentIsReplayable`: signed thinking and a tool call.
pub(crate) fn replayable(content: &[u8]) -> bool {
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

/// `restoreKimiThinkingReplayContent`: puts `cached` (one assistant content array) back
/// into the latest assistant turn whose non-thinking parts it matches. `None` when no turn
/// matches, the turn already carries thinking, or it already equals the cached content.
pub(crate) fn restore_turn(body: &[u8], cached: &[u8]) -> Option<Vec<u8>> {
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

/// Rebuilds the complete assistant content from a Claude SSE stream (or a buffered SSE
/// body): feed every chunk to [`Accumulator::observe`], then [`Accumulator::content`].
#[derive(Default)]
pub(crate) struct Accumulator {
    blocks: BTreeMap<i64, Block>,
    observed: bool,
    complete: bool,
    upstream_error: bool,
    abandoned: bool,
    used: usize,
}

impl Accumulator {
    pub(crate) fn observe(&mut self, chunk: &[u8]) {
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

    /// The complete content array, once a whole message ended without upstream error,
    /// abandonment or an unfinished block.
    pub(crate) fn content(&mut self) -> Option<Vec<u8>> {
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

/// Where a replay stream stores what it observed (Go's per-provider cache and clear
/// functions passed to `wrapThinkingReplayStream`).
pub(crate) trait ReplayTarget: Send + 'static {
    /// Caches complete content (or clears the entry when it is not replayable).
    fn store(&self, content: &[u8]);
    /// Drops the cached entry this request read.
    fn clear(&self);
    /// Whether this request's body was rewritten from the cache.
    fn applied(&self) -> bool;
}

/// `wrapThinkingReplayStream`: forwards every event unchanged; once the stream ends
/// without an error item, caches the complete content, or clears applied replay after an
/// upstream `error` event.
pub(crate) fn wrap_stream<T: ReplayTarget>(stream: ExecStream, target: T) -> ExecStream {
    struct State<T> {
        stream: ExecStream,
        target: T,
        acc: Accumulator,
        failed: bool,
        done: bool,
    }
    futures_util::stream::unfold(
        State {
            stream,
            target,
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
                            st.target.store(&content);
                        } else if st.acc.upstream_error && st.target.applied() {
                            st.target.clear();
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
        let restored = restore_turn(body.as_bytes(), CACHED.as_bytes()).unwrap();
        assert_eq!(gj::get(&restored, "messages.1.content").raw(), CACHED.as_bytes());
        // Already carrying thinking: left alone.
        let with_thinking = gj::try_set_raw(body.as_bytes(), "messages.1.content", CACHED).unwrap();
        assert!(restore_turn(&with_thinking, CACHED.as_bytes()).is_none());
        // Different tool input: not the same turn.
        let other = body.replace(r#""path":"a""#, r#""path":"b""#);
        assert!(restore_turn(other.as_bytes(), CACHED.as_bytes()).is_none());
        // Go EqualFold on the role: "ASSISTANT" still matches.
        let upper = body.replace(r#""role":"assistant""#, r#""role":" ASSISTANT ""#);
        assert!(restore_turn(upper.as_bytes(), CACHED.as_bytes()).is_some());
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

    /// A target that records what the wrapper asked of it.
    #[derive(Clone)]
    struct Recorder {
        log: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
        applied: bool,
    }

    impl ReplayTarget for Recorder {
        fn store(&self, content: &[u8]) {
            self.log
                .lock()
                .unwrap()
                .push(format!("store {}", String::from_utf8_lossy(content)));
        }
        fn clear(&self) {
            self.log.lock().unwrap().push("clear".into());
        }
        fn applied(&self) -> bool {
            self.applied
        }
    }

    async fn run(events: Vec<Result<&'static str, ()>>, applied: bool) -> (usize, Vec<String>) {
        let target = Recorder {
            log: Default::default(),
            applied,
        };
        let items = events.into_iter().map(|e| match e {
            Ok(text) => Ok(bytes::Bytes::from_static(text.as_bytes())),
            Err(()) => Err(cpa_core::exec::ExecError::local(
                502,
                cpa_core::exec::FailureScope::Transport,
                "x",
            )),
        });
        let out: Vec<_> = wrap_stream(futures_util::stream::iter(items).boxed(), target.clone())
            .collect()
            .await;
        let log = target.log.lock().unwrap().clone();
        (out.len(), log)
    }

    #[tokio::test]
    async fn wrapper_stores_complete_content_and_clears_only_applied_errors() {
        // wrapThinkingReplayStream: every item is forwarded; an error item stops caching;
        // a clean end caches complete content, else clears applied replay after an
        // upstream `error` event.
        let start = "data: {\"type\":\"message_start\",\"message\":{}}\n\n";
        let block = "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"hi\"}}\n\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\n";
        let stop = "data: {\"type\":\"message_stop\"}\n\n";
        let error = "data: {\"type\":\"error\",\"error\":{}}\n\n";
        assert_eq!(
            run(vec![Ok(start), Ok(block), Ok(stop)], false).await,
            (3, vec![r#"store [{"type":"text","text":"hi"}]"#.to_owned()])
        );
        assert_eq!(run(vec![Ok(start), Ok(block), Err(())], true).await, (3, vec![]));
        assert_eq!(
            run(vec![Ok(start), Ok(error)], true).await,
            (2, vec!["clear".to_owned()])
        );
        assert_eq!(run(vec![Ok(start), Ok(error)], false).await, (2, vec![]));
    }
}
