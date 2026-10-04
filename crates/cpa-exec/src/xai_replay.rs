//! Stateless Grok reasoning replay (internal/runtime/executor/xai_reasoning_replay.go,
//! internal/cache/xai_reasoning_replay_cache.go and the codex_executor_reasoning.go
//! helpers it reuses).
//!
//! Grok returns encrypted reasoning that clients without server-side state drop from the
//! next turn. After a completed response the executor caches the replayable output items
//! (reasoning, assistant message, tool calls) per (model, session), and the next request
//! of that session gets the items its input lacks inserted before the matching turn.
//!
//! In Home mode the cache lives in Home KV (`cpa:xai:reasoning-replay:*`) as Go writes
//! it, so every node replays the same session state.

use std::collections::{HashMap, HashSet};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use cpa_common::gostr::GoStr;
use cpa_common::json::{self as gj, Kind, Res};
use cpa_common::thinking::parse_suffix;
use cpa_core::exec::ExecRequest;
use cpa_core::format::Format;
use sha2::{Digest, Sha256};

use crate::meta_codex::go_trim_space;
use crate::replay::{claude_code_session, header_session, payload_session};

/// `XAIReasoningReplayCacheTTL`.
const TTL: Duration = Duration::from_secs(3600);
/// `XAIReasoningReplayCacheMaxEntries`.
const MAX_ENTRIES: usize = 10240;
/// `XAIReasoningReplayCacheEvictBatchSize`.
const EVICT_BATCH: usize = 128;

/// `xaiReasoningReplayScope`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct ReplayScope {
    pub model_name: String,
    pub session_key: String,
    /// Home writes the response waits for.
    pub writes: crate::home_replay::Writes,
}

impl ReplayScope {
    fn valid(&self) -> bool {
        !self.model_name.trim().is_empty() && !self.session_key.trim().is_empty()
    }
}

struct Entry {
    items: Vec<Vec<u8>>,
    timestamp: Instant,
}

/// The in-process replay cache (Go's package-level map), used outside Home mode.
// ponytail: Go also purges expired entries from a background ticker; here an expired
// entry is dropped when read and capacity eviction bounds memory.
#[derive(Default)]
pub(crate) struct Store {
    entries: Mutex<HashMap<String, Entry>>,
}

/// `xaiReasoningReplayCacheKey`.
fn cache_key(scope: &ReplayScope) -> Option<String> {
    let (model, session) = (scope.model_name.trim(), scope.session_key.trim());
    (!model.is_empty() && !session.is_empty()).then(|| format!("xai-reasoning-replay\0{model}\0{session}"))
}

impl Store {
    /// `GetXAIReasoningReplayItemsRequired`: a hit refreshes the entry's TTL.
    fn get(&self, scope: &ReplayScope) -> Option<Vec<Vec<u8>>> {
        let key = cache_key(scope)?;
        let mut entries = self.entries.lock().unwrap_or_else(|e| e.into_inner());
        let now = Instant::now();
        let entry = entries.get_mut(&key)?;
        if now.duration_since(entry.timestamp) > TTL {
            entries.remove(&key);
            return None;
        }
        entry.timestamp = now;
        Some(entry.items.clone())
    }

    /// `StoreXAIReasoningReplayItems`: false when the items hold no replay anchor.
    fn store(&self, scope: &ReplayScope, items: &[Vec<u8>]) -> bool {
        let Some(key) = cache_key(scope) else {
            return false;
        };
        let Some(normalized) = normalize_items(items) else {
            return false;
        };
        let mut entries = self.entries.lock().unwrap_or_else(|e| e.into_inner());
        entries.insert(
            key,
            Entry {
                items: normalized,
                timestamp: Instant::now(),
            },
        );
        if entries.len() > MAX_ENTRIES {
            let mut oldest: Vec<(Instant, String)> = entries.iter().map(|(k, e)| (e.timestamp, k.clone())).collect();
            oldest.sort();
            for (_, k) in oldest.into_iter().take(EVICT_BATCH) {
                entries.remove(&k);
            }
        }
        true
    }

    /// `DeleteXAIReasoningReplayItemRequired`.
    fn delete(&self, scope: &ReplayScope) {
        if let Some(key) = cache_key(scope) {
            self.entries.lock().unwrap_or_else(|e| e.into_inner()).remove(&key);
        }
    }
}

/// Go `XAIReasoningReplayStoreStatus`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StoreStatus {
    InvalidArgs = 0,
    Stored = 1,
    NoReplayableState = 2,
    BackendError = 3,
}

/// `xaiReasoningReplayKVKey`.
fn kv_key(scope: &ReplayScope) -> String {
    format!(
        "cpa:xai:reasoning-replay:{}:{}",
        cpa_home::kv::hash_key_part(scope.model_name.trim()),
        cpa_home::kv::hash_key_part(scope.session_key.trim())
    )
}

/// Go `StoreXAIReasoningReplayItems` in Home mode: the normalized items as Go's
/// `json.Marshal([][]byte)` (base64 strings), kept for an hour.
async fn home_store(client: &cpa_home::Client, scope: &ReplayScope, items: &[Vec<u8>]) -> StoreStatus {
    use base64::Engine;
    if cache_key(scope).is_none() {
        return StoreStatus::InvalidArgs;
    }
    let Some(normalized) = normalize_items(items) else {
        return StoreStatus::NoReplayableState;
    };
    let encoded: Vec<String> = normalized
        .iter()
        .map(|item| base64::engine::general_purpose::STANDARD.encode(item))
        .collect();
    let raw = serde_json::to_vec(&encoded).unwrap_or_default();
    let options = cpa_home::SetOptions {
        ex: TTL,
        ..Default::default()
    };
    match client.kv_set(&kv_key(scope), &raw, options).await {
        Ok(true) => StoreStatus::Stored,
        Ok(false) => StoreStatus::BackendError,
        Err(error) => {
            tracing::error!("home kv best-effort xai reasoning replay set failed prefix=cpa:xai:*: {error}");
            StoreStatus::BackendError
        }
    }
}

/// Go `GetXAIReasoningReplayItemsRequired` in Home mode: a hit renews the TTL.
async fn home_get(client: &cpa_home::Client, scope: &ReplayScope) -> Result<Option<Vec<Vec<u8>>>, String> {
    if cache_key(scope).is_none() {
        return Ok(None);
    }
    let key = kv_key(scope);
    let Some(raw) = client.kv_get(&key).await.map_err(|e| e.to_string())? else {
        return Ok(None);
    };
    let items = cpa_home::kv::decode_byte_slices(&raw)?;
    if let Err(error) = client.kv_expire(&key, TTL).await {
        tracing::warn!("home kv xai reasoning replay expire failed prefix=cpa:xai:*: {error}");
    }
    Ok(Some(items))
}

/// Go `DeleteXAIReasoningReplayItemRequired` in Home mode.
async fn home_delete(client: &cpa_home::Client, scope: &ReplayScope) -> Result<(), String> {
    if cache_key(scope).is_none() {
        return Ok(());
    }
    client
        .kv_del(&[&kv_key(scope)])
        .await
        .map(drop)
        .map_err(|e| e.to_string())
}

/// `strings.TrimSpace(r.String())`.
fn text(r: &Res<'_>) -> String {
    String::from_utf8_lossy(go_trim_space(&r.bytes())).into_owned()
}

/// `xaiReasoningReplayScopeFromRequest`: Claude and OpenAI Responses clients only, and
/// not for a downstream WebSocket turn that continues upstream state.
pub(crate) fn scope(req: &ExecRequest, body: &[u8], downstream_websocket: bool) -> ReplayScope {
    if !matches!(req.source_format, Format::Claude | Format::OpenAIResponse) {
        return ReplayScope::default();
    }
    if downstream_websocket && !text(&gj::get(&req.body, "previous_response_id")).is_empty() {
        return ReplayScope::default();
    }
    ReplayScope {
        model_name: parse_suffix(&req.model).model_name,
        session_key: isolate(req, &session_key(req, body)),
        writes: Default::default(),
    }
}

/// `codexReasoningReplaySessionKey` for the sources replay serves.
fn session_key(req: &ExecRequest, body: &[u8]) -> String {
    if req.source_format == Format::Claude
        && let Some(key) = claude_code_session(&req.body, &req.headers)
    {
        return key;
    }
    if let Some(execution) = req
        .execution_session
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        return format!("execution:{execution}");
    }
    payload_session(body)
        .or_else(|| payload_session(&req.body))
        .or_else(|| header_session(&req.headers))
        .unwrap_or_default()
}

/// `xaiReasoningReplayIsolateSessionKey`: client-controlled keys are namespaced by the
/// caller's API key, and disabled without one.
fn isolate(req: &ExecRequest, key: &str) -> String {
    let key = key.trim();
    if key.is_empty() || key.starts_with("execution:") {
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

/// `applyXAIReasoningReplayCacheRequired` after the scope is known. A read failure
/// (Home KV unavailable) only skips the replay, as in Go.
pub(crate) async fn apply(store: &Store, scope: &ReplayScope, body: Vec<u8>) -> Vec<u8> {
    if !scope.valid() {
        return body;
    }
    let read = match cpa_home::kv::current_client() {
        Ok(None) => Ok(store.get(scope)),
        Ok(Some(client)) => home_get(&client, scope).await,
        Err(error) => Err(error.to_string()),
    };
    let items = match read {
        Ok(Some(items)) => items,
        Ok(None) => return body,
        Err(error) => {
            tracing::warn!("xai reasoning replay cache read failed: {error}");
            return body;
        }
    };
    let items = filter_for_input(&body, items);
    if items.is_empty() {
        return body;
    }
    insert_items(&body, &items).unwrap_or(body)
}

/// `cacheXAIReasoningReplayFromCompleted`: a completed turn without replayable state
/// clears the session so an older turn's reasoning is never injected later.
pub(crate) fn cache_completed(store: &Store, scope: &ReplayScope, completed: &[u8]) {
    if !scope.valid() {
        return;
    }
    let output = gj::get(completed, "response.output");
    if !output.is_array() {
        return;
    }
    let items: Vec<Vec<u8>> = output
        .array()
        .iter()
        .filter(|item| {
            matches!(
                text(&item.get("type")).as_str(),
                "reasoning" | "message" | "function_call" | "custom_tool_call"
            )
        })
        .map(|item| item.raw().to_vec())
        .collect();
    match cpa_home::kv::current_client() {
        Ok(None) => {
            if !store.store(scope, &items) {
                store.delete(scope);
            }
        }
        // Go: a backend error keeps the previous entry.
        Err(error) => {
            tracing::error!("home kv best-effort xai reasoning replay set failed prefix=cpa:xai:*: {error}");
        }
        Ok(Some(client)) => {
            let writes = scope.writes.clone();
            let scope = scope.clone();
            writes.spawn(async move {
                if home_store(&client, &scope, &items).await == StoreStatus::NoReplayableState
                    && let Err(error) = home_delete(&client, &scope).await
                {
                    tracing::warn!(
                        "xai reasoning replay cache delete failed after non-replayable completed output: {error}"
                    );
                }
            });
        }
    }
}

/// `clearXAIReasoningReplayAfterCompaction`.
pub(crate) async fn clear(store: &Store, scope: &ReplayScope) {
    if !scope.valid() {
        return;
    }
    let deleted = match cpa_home::kv::current_client() {
        Ok(None) => {
            store.delete(scope);
            Ok(())
        }
        Ok(Some(client)) => home_delete(&client, scope).await,
        Err(error) => Err(error.to_string()),
    };
    if let Err(error) = deleted {
        tracing::warn!("xai reasoning replay cache delete failed after successful compaction: {error}");
    }
}

// --- cache normalization (normalizeXAIReasoningReplayItems) -----------------------------

/// The normalized items, or `None` when none is a reasoning item or tool call.
fn normalize_items(items: &[Vec<u8>]) -> Option<Vec<Vec<u8>>> {
    let mut normalized = Vec::with_capacity(items.len());
    let mut anchor = false;
    for item in items {
        if let Some(n) = normalize_item(item) {
            if matches!(
                text(&gj::get(&n, "type")).as_str(),
                "reasoning" | "function_call" | "custom_tool_call"
            ) {
                anchor = true;
            }
            normalized.push(n);
        }
    }
    anchor.then_some(normalized)
}

fn normalize_item(item: &[u8]) -> Option<Vec<u8>> {
    let r = gj::parse(item);
    match text(&r.get("type")).as_str() {
        "reasoning" => {
            let encrypted = r.get("encrypted_content");
            if encrypted.kind != Kind::String {
                return None;
            }
            let value = encrypted.bytes();
            if go_trim_space(&value) != &*value
                || cpa_common::signature::inspect_grok_encrypted_content(&*value).is_err()
            {
                return None;
            }
            let mut out = br#"{"type":"reasoning","summary":[],"content":null}"#.to_vec();
            gj::set_str(&mut out, "encrypted_content", &*value);
            Some(out)
        }
        "message" => {
            if !text(&r.get("role")).go_eq_fold("assistant") {
                return None;
            }
            let content = r.get("content");
            if !content.is_array() || content.array().is_empty() {
                return None;
            }
            let mut out = br#"{"type":"message","role":"assistant","content":[]}"#.to_vec();
            let mut parts = 0;
            for part in content.array() {
                let next = match text(&part.get("type")).as_str() {
                    "output_text" => {
                        let value = part.get("text");
                        if value.kind != Kind::String {
                            continue;
                        }
                        let mut p = br#"{"type":"output_text","text":""}"#.to_vec();
                        gj::set_str(&mut p, "text", &*value.bytes());
                        p
                    }
                    "refusal" => {
                        let value = part.get("refusal");
                        if value.kind != Kind::String {
                            continue;
                        }
                        let mut p = br#"{"type":"refusal","refusal":""}"#.to_vec();
                        gj::set_str(&mut p, "refusal", &*value.bytes());
                        p
                    }
                    _ => continue,
                };
                if !gj::set_raw(&mut out, "content.-1", next) {
                    return None;
                }
                parts += 1;
            }
            (parts > 0).then_some(out)
        }
        "function_call" => {
            let (call_id, name, arguments) = (text(&r.get("call_id")), text(&r.get("name")), r.get("arguments"));
            if call_id.is_empty() || name.is_empty() || arguments.kind != Kind::String {
                return None;
            }
            let mut out = br#"{"type":"function_call"}"#.to_vec();
            gj::set_str(&mut out, "call_id", call_id);
            gj::set_str(&mut out, "name", name);
            gj::set_str(&mut out, "arguments", &*arguments.bytes());
            Some(out)
        }
        "custom_tool_call" => {
            let (call_id, name, input) = (text(&r.get("call_id")), text(&r.get("name")), r.get("input"));
            if call_id.is_empty() || name.is_empty() || !input.exists() {
                return None;
            }
            let mut out = br#"{"type":"custom_tool_call","status":"completed"}"#.to_vec();
            let status = text(&r.get("status"));
            if !status.is_empty() {
                gj::set_str(&mut out, "status", status);
            }
            gj::set_str(&mut out, "call_id", call_id);
            gj::set_str(&mut out, "name", name);
            if input.kind == Kind::String {
                gj::set_str(&mut out, "input", &*input.bytes());
            } else {
                gj::set_raw(&mut out, "input", input.raw());
            }
            Some(out)
        }
        _ => None,
    }
}

// --- request side (filterXAIReasoningReplayItemsForInput, insertCodexReasoningReplayItems) -

/// `util.SanitizeClaudeToolID` for a non-empty id: every rune outside `[A-Za-z0-9_-]`
/// (an invalid byte counts as one rune) becomes `_`.
fn sanitize_tool_id(id: &str) -> String {
    let bytes = id.as_bytes();
    let mut out = String::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let (c, n) = gj::decode_rune(&bytes[i..]);
        match c {
            Some(c) if c.is_ascii_alphanumeric() || c == '_' || c == '-' => out.push(c),
            _ => out.push('_'),
        }
        i += n.max(1);
    }
    out
}

/// `shortenCodexReplayCallIDIfNeeded`.
fn shorten_call_id(id: String) -> String {
    const LIMIT: usize = 64;
    if id.len() <= LIMIT {
        return id;
    }
    let digest = Sha256::digest(id.as_bytes());
    let suffix: String = std::iter::once('_')
        .chain(
            digest[..8]
                .iter()
                .flat_map(|b| format!("{b:02x}").chars().collect::<Vec<_>>()),
        )
        .collect();
    format!("{}{suffix}", &id[..LIMIT - suffix.len()])
}

/// `codexReplayComparableCallIDs`: the id and its Claude-visible form.
fn comparable_call_ids(call_id: &str) -> Vec<String> {
    let call_id = call_id.trim();
    if call_id.is_empty() {
        return vec![];
    }
    let visible = shorten_call_id(sanitize_tool_id(call_id));
    if visible.is_empty() || visible == call_id {
        vec![call_id.to_owned()]
    } else {
        vec![call_id.to_owned(), visible]
    }
}

/// `codexReplayToolCallKeys`.
fn tool_call_keys(item: &Res<'_>) -> Vec<String> {
    let item_type = text(&item.get("type"));
    if item_type != "function_call" && item_type != "custom_tool_call" {
        return vec![];
    }
    comparable_call_ids(&item.get("call_id").str())
        .into_iter()
        .map(|id| format!("{item_type}:{id}"))
        .collect()
}

/// One part of an assistant message, for equality (`xaiAssistantMessagePart`).
fn message_parts(content: &Res<'_>) -> Option<Vec<(String, Vec<u8>)>> {
    if content.kind == Kind::String {
        return Some(vec![("output_text".into(), content.bytes().into_owned())]);
    }
    if !content.is_array() {
        return None;
    }
    let mut parts = vec![];
    for part in content.array() {
        let part_type = text(&part.get("type"));
        let field = match part_type.as_str() {
            "output_text" => "text",
            "refusal" => "refusal",
            _ => return None,
        };
        let value = part.get(field);
        if value.kind != Kind::String {
            return None;
        }
        parts.push((part_type, value.bytes().into_owned()));
    }
    (!parts.is_empty()).then_some(parts)
}

fn message_content_equal(left: &Res<'_>, right: &Res<'_>) -> bool {
    match (message_parts(left), message_parts(right)) {
        (Some(l), Some(r)) => l == r,
        _ => false,
    }
}

fn is_assistant(item: &Res<'_>) -> bool {
    text(&item.get("role")).go_eq_fold("assistant")
}

/// `filterXAIReasoningReplayItemsForInput`: the cached items the input still lacks.
fn filter_for_input(body: &[u8], items: Vec<Vec<u8>>) -> Vec<Vec<u8>> {
    let input = gj::get(body, "input");
    if !input.is_array() {
        return vec![];
    }
    let input_items = input.array();
    let last_assistant = input_items.iter().rev().find(|item| {
        let t = text(&item.get("type"));
        (t.is_empty() || t == "message") && is_assistant(item)
    });
    let parsed: Vec<Res<'_>> = items.iter().map(|i| gj::parse(i)).collect();
    let cached_assistant = parsed
        .iter()
        .find(|item| text(&item.get("type")) == "message" && is_assistant(item));
    let message_matches = match (last_assistant, cached_assistant) {
        (Some(last), Some(cached)) => {
            if !message_content_equal(&last.get("content"), &cached.get("content")) {
                // Ambiguous history: the client's last assistant turn is not ours.
                return vec![];
            }
            true
        }
        _ => false,
    };
    let mut existing_calls = HashSet::new();
    let mut existing_outputs = HashSet::new();
    for item in &input_items {
        let item_type = text(&item.get("type"));
        if item_type == "function_call_output" || item_type == "custom_tool_call_output" {
            existing_outputs.extend(comparable_call_ids(&item.get("call_id").str()));
        }
        existing_calls.extend(tool_call_keys(item));
    }
    let mut filtered = vec![];
    for (item, r) in items.iter().zip(&parsed) {
        match text(&r.get("type")).as_str() {
            "reasoning" => {
                let encrypted = r.get("encrypted_content").bytes();
                let present = !encrypted.is_empty()
                    && input_items.iter().any(|i| {
                        let e = i.get("encrypted_content");
                        text(&i.get("type")) == "reasoning" && e.kind == Kind::String && *e.bytes() == *encrypted
                    });
                if present {
                    continue;
                }
            }
            "message" => {
                if message_matches {
                    continue;
                }
            }
            "function_call" | "custom_tool_call" => {
                let keys = tool_call_keys(r);
                if keys.is_empty() || keys.iter().any(|k| existing_calls.contains(k)) {
                    continue;
                }
                if !comparable_call_ids(&r.get("call_id").str())
                    .iter()
                    .any(|id| existing_outputs.contains(id))
                {
                    continue;
                }
                existing_calls.extend(keys);
            }
            _ => continue,
        }
        filtered.push(item.clone());
    }
    filtered
}

/// `codexReplayMessageRole`.
fn message_role(item: &Res<'_>) -> Option<String> {
    let item_type = text(&item.get("type"));
    let role = text(&item.get("role")).go_lower();
    (!role.is_empty() && (item_type.is_empty() || item_type == "message")).then_some(role)
}

fn is_call_output(item: &Res<'_>) -> bool {
    matches!(
        text(&item.get("type")).as_str(),
        "function_call_output" | "custom_tool_call_output"
    )
}

/// `codexReasoningReplayInsertIndex`.
fn insert_index(input: &[Res<'_>], replay: &[Res<'_>]) -> usize {
    let mut replay_ids = HashSet::new();
    for item in replay {
        if matches!(text(&item.get("type")).as_str(), "function_call" | "custom_tool_call") {
            replay_ids.extend(comparable_call_ids(&item.get("call_id").str()));
        }
    }
    if !replay_ids.is_empty()
        && let Some(index) = input.iter().position(|item| {
            let call_id = text(&item.get("call_id"));
            is_call_output(item) && (call_id.is_empty() || replay_ids.contains(&call_id))
        })
    {
        return index;
    }
    if let Some(index) = input
        .iter()
        .rposition(|item| message_role(item).as_deref() == Some("assistant"))
    {
        return index;
    }
    input
        .iter()
        .position(|item| !matches!(message_role(item).as_deref(), Some("developer" | "system")))
        .unwrap_or(input.len())
}

/// `codexAlignReasoningReplayToolCallIDs`: replayed calls take the call id their output
/// carries in the input.
fn align_call_ids(input: &[Res<'_>], replay: &[Vec<u8>]) -> Vec<Vec<u8>> {
    let mut outputs: HashMap<String, String> = HashMap::new();
    for item in input.iter().filter(|i| is_call_output(i)) {
        let call_id = text(&item.get("call_id"));
        if call_id.is_empty() {
            continue;
        }
        for candidate in comparable_call_ids(&call_id) {
            outputs.insert(candidate, call_id.clone());
        }
    }
    if outputs.is_empty() {
        return replay.to_vec();
    }
    replay
        .iter()
        .map(|item| {
            let r = gj::parse(item);
            if !matches!(text(&r.get("type")).as_str(), "function_call" | "custom_tool_call") {
                return item.clone();
            }
            let call_id = text(&r.get("call_id"));
            let output = comparable_call_ids(&call_id)
                .into_iter()
                .find_map(|c| outputs.get(&c).filter(|v| !v.is_empty()).cloned());
            match output {
                Some(output) if output != call_id => {
                    gj::try_set_str(item, "call_id", output).unwrap_or_else(|_| item.clone())
                }
                _ => item.clone(),
            }
        })
        .collect()
}

/// `insertCodexReasoningReplayItems`.
fn insert_items(body: &[u8], replay: &[Vec<u8>]) -> Option<Vec<u8>> {
    let input = gj::get(body, "input");
    if !input.is_array() || replay.is_empty() {
        return None;
    }
    let input_items = input.array();
    let parsed: Vec<Res<'_>> = replay.iter().map(|i| gj::parse(i)).collect();
    let index = insert_index(&input_items, &parsed);
    let replay = align_call_ids(&input_items, replay);
    let mut items: Vec<&[u8]> = Vec::with_capacity(input_items.len() + replay.len());
    for (i, item) in input_items.iter().enumerate() {
        if i == index {
            items.extend(replay.iter().map(Vec::as_slice));
        }
        items.push(item.raw());
    }
    if index == input_items.len() {
        items.extend(replay.iter().map(Vec::as_slice));
    }
    let mut raw = b"[".to_vec();
    raw.extend_from_slice(&items.join(&b","[..]));
    raw.push(b']');
    gj::try_set_raw(body, "input", raw).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn long_call_ids_shorten_like_go() {
        // 70 sanitized characters: 47-byte prefix plus `_` and 16 hex digits.
        let id = "call.".to_owned() + &"x".repeat(65);
        let ids = comparable_call_ids(&id);
        assert_eq!(ids.len(), 2);
        assert_eq!(ids[1].len(), 64);
        assert!(ids[1].starts_with("call_xxx"));
        assert_eq!(comparable_call_ids("  call_1 "), vec!["call_1".to_owned()]);
    }

    /// Go's Home KV backend for xAI replay, recorded by the reference's
    /// internal/cache/zz_rustgolden_test.go: keys, base64 values, TTLs, the store
    /// statuses and the items read back.
    #[tokio::test]
    async fn home_kv_replay_matches_go() {
        use std::sync::{Arc, Mutex};
        let golden: serde_json::Value =
            serde_json::from_str(include_str!("claude/testdata/go_xai_replay_home.json")).unwrap();
        let steps = golden["steps"].as_array().unwrap();
        let values = Arc::new(Mutex::new(HashMap::new()));
        let home = cpa_home::fake::FakeHome::start(crate::claude::kv_test::kv_home(values)).await;
        let client = home.client();
        let scope = |model: &str| ReplayScope {
            model_name: model.into(),
            session_key: " sess-1 ".into(),
            writes: Default::default(),
        };
        let anchored: Vec<Vec<u8>> = [
            r#"{"type":"message","id":"m1","role":"assistant","status":"completed","content":[{"type":"output_text","text":"<hi> & bye","annotations":[]}]}"#,
            r#"{"type":"function_call","id":"fc1","call_id":"call_1","name":"lookup","arguments":"{\"q\":1}","status":"completed"}"#,
            r#"{"type":"web_search_call","id":"ws1"}"#,
        ]
        .iter()
        .map(|s| s.as_bytes().to_vec())
        .collect();
        let mut seen = 0;
        let mut calls = || {
            let all: Vec<serde_json::Value> = home
                .commands()
                .iter()
                .filter_map(|c| crate::claude::kv_test::as_go_call(c))
                .collect();
            let new = all[seen..].to_vec();
            seen = all.len();
            serde_json::Value::from(new)
        };
        let status = home_store(&client, &scope(" grok-4 "), &anchored).await;
        assert_eq!(status as i64, steps[0]["result"], "store_anchored");
        assert_eq!(calls(), steps[0]["calls"], "store_anchored");
        let items = home_get(&client, &scope("grok-4")).await.unwrap().unwrap();
        let texts: Vec<String> = items.iter().map(|i| String::from_utf8(i.clone()).unwrap()).collect();
        assert_eq!(serde_json::json!(texts), steps[1]["result"]["items"], "get");
        assert_eq!(calls(), steps[1]["calls"], "get");
        let status = home_store(&client, &scope("grok-4"), &anchored[..1]).await;
        assert_eq!(status as i64, steps[2]["result"], "store_unanchored");
        assert_eq!(calls(), steps[2]["calls"], "store_unanchored");
        home_delete(&client, &scope("grok-4")).await.unwrap();
        assert_eq!(calls(), steps[3]["calls"], "delete");
        assert!(home_get(&client, &scope("grok-4")).await.unwrap().is_none());
        assert_eq!(calls(), steps[4]["calls"], "get_missing");
        let status = home_store(&client, &scope(""), &anchored).await;
        assert_eq!(status as i64, steps[5]["result"], "store_invalid_scope");
        assert_eq!(calls(), steps[5]["calls"], "store_invalid_scope");
    }
}
