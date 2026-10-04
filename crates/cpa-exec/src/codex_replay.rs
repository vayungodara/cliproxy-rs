//! Codex reasoning replay for Claude clients (codex_executor_reasoning.go and
//! internal/cache/codex_reasoning_replay_cache.go).
//!
//! Claude clients drop Codex's encrypted reasoning and tool-call items from history.
//! After each completed turn the executor caches those items per (model, Claude Code
//! agent or session), behind a marker that fingerprints the request input and the
//! assistant message, and re-inserts them before the matching turn of later requests.
//! An upstream `thinking_signature_invalid` rejection clears the entry.
//!
//! In Home mode the entry lives in Home KV (`cpa:codex:reasoning-replay:*`) as Go's
//! `json.Marshal([][]byte)`, appended by compare-and-swap.

use std::collections::{HashMap, HashSet};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use cpa_common::gostr::trim_space;
use cpa_common::json::{self as gj, Kind, Res};
use cpa_core::exec::ExecRequest;
use cpa_core::format::Format;
use sha2::{Digest, Sha256};

/// `CodexReasoningReplayTurnType`: the internal turn-boundary marker.
const TURN_TYPE: &str = "cpa_codex_replay_turn";
const TTL: Duration = Duration::from_secs(3600);
const MAX_ENTRIES: usize = 10240;
const MAX_TURNS_PER_ENTRY: usize = 256;
const MAX_BYTES_PER_ENTRY: usize = 16 << 20;
const EVICT_BATCH: usize = 128;

/// `strings.TrimSpace(r.String())`.
fn text(r: &Res<'_>) -> String {
    String::from_utf8_lossy(trim_space(&r.bytes())).into_owned()
}

fn field(item: &Res<'_>, key: &str) -> String {
    text(&item.get(key))
}

fn item_type(item: &[u8]) -> String {
    text(&gj::get(item, "type"))
}

// ------------------------------------------------------------------------------- cache

struct Entry {
    items: Vec<Vec<u8>>,
    touched: Instant,
}

/// The in-process replay store (Go's non-Home mode).
// ponytail: expired entries are dropped when read and evicted at capacity; Go also purges
// them on a timer.
#[derive(Default)]
pub(crate) struct Cache {
    entries: Mutex<HashMap<String, Entry>>,
}

fn cache_key(model: &str, session: &str) -> Option<String> {
    let (model, session) = (model.trim(), session.trim());
    (!model.is_empty() && !session.is_empty()).then(|| format!("codex-reasoning-replay\0{model}\0{session}"))
}

impl Cache {
    /// `GetCodexReasoningReplayItemsRequired`: the entry, with its TTL refreshed.
    pub(crate) fn get(&self, model: &str, session: &str) -> Option<Vec<Vec<u8>>> {
        let key = cache_key(model, session)?;
        let mut entries = self.entries.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let entry = entries.get_mut(&key)?;
        if entry.touched.elapsed() > TTL {
            entries.remove(&key);
            return None;
        }
        entry.touched = Instant::now();
        Some(entry.items.clone())
    }

    /// `AppendCodexReasoningReplayItemsBestEffort`: one normalized turn appended.
    pub(crate) fn append(&self, model: &str, session: &str, items: &[Vec<u8>]) -> bool {
        let Some(key) = cache_key(model, session) else {
            return false;
        };
        let Some(normalized) = normalize_turn(items) else {
            return false;
        };
        let mut entries = self.entries.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let now = Instant::now();
        let entry = entries.entry(key).or_insert_with(|| Entry {
            items: Vec::new(),
            touched: now,
        });
        if entry.touched.elapsed() > TTL {
            entry.items.clear();
        }
        entry.items = append_turn(std::mem::take(&mut entry.items), normalized);
        entry.touched = now;
        if entries.len() > MAX_ENTRIES {
            let mut oldest: Vec<(Instant, String)> = entries.iter().map(|(k, e)| (e.touched, k.clone())).collect();
            oldest.sort();
            for (_, key) in oldest.into_iter().take(EVICT_BATCH) {
                entries.remove(&key);
            }
        }
        true
    }

    fn delete(&self, model: &str, session: &str) {
        if let Some(key) = cache_key(model, session) {
            self.entries
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .remove(&key);
        }
    }
}

/// `normalizeCodexReasoningReplayItems`: `None` when nothing replayable remains.
fn normalize_turn(items: &[Vec<u8>]) -> Option<Vec<Vec<u8>>> {
    let normalized = trim(items.iter().filter_map(|i| normalize(i)).collect());
    (!normalized.is_empty()).then_some(normalized)
}

/// `codexReasoningReplayKVKey`.
fn kv_key(model: &str, session: &str) -> String {
    format!(
        "cpa:codex:reasoning-replay:{}:{}",
        cpa_home::kv::hash_key_part(model.trim()),
        cpa_home::kv::hash_key_part(session.trim())
    )
}

/// Go `json.Marshal([][]byte)` / `json.Unmarshal`: base64 strings.
fn encode_items(items: &[Vec<u8>]) -> Vec<u8> {
    use base64::Engine;
    let encoded: Vec<String> = items
        .iter()
        .map(|item| base64::engine::general_purpose::STANDARD.encode(item))
        .collect();
    serde_json::to_vec(&encoded).unwrap_or_default()
}

fn decode_items(raw: &[u8]) -> Result<Vec<Vec<u8>>, String> {
    cpa_home::kv::decode_byte_slices(raw)
}

/// Go `GetCodexReasoningReplayItemsRequired` in Home mode; a failed TTL renewal is an
/// error, as in Go.
async fn home_get(client: &cpa_home::Client, model: &str, session: &str) -> Result<Option<Vec<Vec<u8>>>, String> {
    if cache_key(model, session).is_none() {
        return Ok(None);
    }
    let key = kv_key(model, session);
    let Some(raw) = client.kv_get(&key).await.map_err(|e| e.to_string())? else {
        return Ok(None);
    };
    let items = decode_items(&raw)?;
    client.kv_expire(&key, TTL).await.map_err(|e| e.to_string())?;
    Ok(Some(items))
}

/// Go `AppendCodexReasoningReplayItemsBestEffort` in Home mode: read, append, and
/// compare-and-swap, up to 32 attempts against racing writers.
async fn home_append(client: &cpa_home::Client, model: &str, session: &str, items: &[Vec<u8>]) -> bool {
    if cache_key(model, session).is_none() {
        return false;
    }
    let Some(normalized) = normalize_turn(items) else {
        return false;
    };
    let key = kv_key(model, session);
    let failed = |error: String| {
        tracing::error!("home kv best-effort codex reasoning replay append failed prefix=cpa:codex:*: {error}");
        false
    };
    for _ in 0..32 {
        let existing = match client.kv_get(&key).await {
            Ok(existing) => existing,
            Err(error) => return failed(error.to_string()),
        };
        let current = match existing.as_deref().map(decode_items).transpose() {
            Ok(current) => current.unwrap_or_default(),
            Err(error) => return failed(error),
        };
        let combined = encode_items(&append_turn(current, normalized.clone()));
        match client
            .kv_compare_and_swap(&key, existing.as_deref(), &combined, TTL)
            .await
        {
            Ok(true) => return true,
            Ok(false) => {}
            Err(error) => return failed(error.to_string()),
        }
    }
    tracing::warn!("home kv best-effort codex reasoning replay append exhausted compare-and-swap attempts");
    false
}

/// Go `DeleteCodexReasoningReplayItemRequired` in Home mode.
async fn home_delete(client: &cpa_home::Client, model: &str, session: &str) -> Result<(), String> {
    if cache_key(model, session).is_none() {
        return Ok(());
    }
    client
        .kv_del(&[&kv_key(model, session)])
        .await
        .map(drop)
        .map_err(|e| e.to_string())
}

/// `appendCodexReasoningReplayTurn`: a turn whose marker id is already stored is not
/// appended again.
fn append_turn(mut existing: Vec<Vec<u8>>, turn: Vec<Vec<u8>>) -> Vec<Vec<u8>> {
    if existing.first().is_some_and(|i| item_type(i) != TURN_TYPE) {
        existing.clear();
    }
    let turn_id = turn
        .first()
        .filter(|i| item_type(i) == TURN_TYPE)
        .map(|i| text(&gj::get(i, "id")))
        .unwrap_or_default();
    let known = !turn_id.is_empty()
        && existing
            .iter()
            .any(|i| item_type(i) == TURN_TYPE && text(&gj::get(i, "id")) == turn_id);
    if !known {
        existing.extend(turn);
    }
    trim(existing)
}

/// `trimCodexReasoningReplayItems`: oldest turns go until the entry fits.
fn trim(mut items: Vec<Vec<u8>>) -> Vec<Vec<u8>> {
    loop {
        let mut starts = vec![0];
        let mut total = 0;
        for (index, item) in items.iter().enumerate() {
            total += item.len();
            if index > 0 && item_type(item) == TURN_TYPE {
                starts.push(index);
            }
        }
        if starts.len() <= MAX_TURNS_PER_ENTRY && total <= MAX_BYTES_PER_ENTRY {
            return items;
        }
        if starts.len() <= 1 {
            return Vec::new();
        }
        items.drain(..starts[1]);
    }
}

/// `normalizeCodexReasoningReplayItem`: the minimal shape Responses input replay accepts.
fn normalize(item: &[u8]) -> Option<Vec<u8>> {
    let r = gj::parse(item);
    match item_type(item).as_str() {
        TURN_TYPE => {
            let id = field(&r, "id");
            if id.is_empty() {
                return None;
            }
            let mut out = format!(r#"{{"type":"{TURN_TYPE}"}}"#).into_bytes();
            gj::set_str(&mut out, "id", &id);
            for key in ["assistant_fingerprint", "request_fingerprint"] {
                let value = field(&r, key);
                if !value.is_empty() {
                    gj::set_str(&mut out, key, &value);
                }
            }
            let call_ids = r.get("call_ids");
            if call_ids.is_array() {
                for id in call_ids.array() {
                    let id = text(&id);
                    if !id.is_empty() {
                        gj::set_str(&mut out, "call_ids.-1", &id);
                    }
                }
            }
            Some(out)
        }
        "reasoning" => {
            let encrypted = r.get("encrypted_content");
            if encrypted.kind != Kind::String {
                return None;
            }
            let value = encrypted.bytes();
            if trim_space(&value) != &value[..] || !cpa_common::signature::is_valid_gpt_reasoning_signature(&value[..])
            {
                return None;
            }
            let mut out = br#"{"type":"reasoning","summary":[],"content":null}"#.to_vec();
            gj::set_str(&mut out, "encrypted_content", &value[..]);
            Some(out)
        }
        "function_call" => {
            let (call_id, name) = (field(&r, "call_id"), field(&r, "name"));
            let arguments = r.get("arguments");
            if call_id.is_empty() || name.is_empty() || arguments.kind != Kind::String {
                return None;
            }
            let mut out = br#"{"type":"function_call"}"#.to_vec();
            gj::set_str(&mut out, "call_id", &call_id);
            gj::set_str(&mut out, "name", &name);
            gj::set_str(&mut out, "arguments", &arguments.bytes()[..]);
            Some(out)
        }
        "custom_tool_call" => {
            let (call_id, name) = (field(&r, "call_id"), field(&r, "name"));
            let input = r.get("input");
            if call_id.is_empty() || name.is_empty() || !input.exists() {
                return None;
            }
            let mut out = br#"{"type":"custom_tool_call","status":"completed"}"#.to_vec();
            let status = field(&r, "status");
            if !status.is_empty() {
                gj::set_str(&mut out, "status", &status);
            }
            gj::set_str(&mut out, "call_id", &call_id);
            gj::set_str(&mut out, "name", &name);
            if input.kind == Kind::String {
                gj::set_str(&mut out, "input", &input.bytes()[..]);
            } else {
                gj::set_raw(&mut out, "input", input.raw());
            }
            Some(out)
        }
        _ => None,
    }
}

// ------------------------------------------------------------------------------- scope

/// Where one request's replay lives (`codexReasoningReplayScope`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Scope {
    model: String,
    session: String,
    request_fingerprint: String,
    /// Home writes the response waits for.
    pub(crate) writes: crate::home_replay::Writes,
}

impl Scope {
    fn valid(&self) -> bool {
        !self.model.trim().is_empty() && !self.session.trim().is_empty()
    }
}

/// `codexReasoningReplayScopeFromRequest`: Claude clients only.
fn scope(req: &ExecRequest, body: &[u8]) -> Scope {
    if req.source_format != Format::Claude {
        return Scope::default();
    }
    let mut model = text(&gj::get(body, "model"));
    if model.is_empty() {
        model = cpa_common::thinking::parse_suffix(&req.model).model_name;
    }
    let items = gj::get(body, "input").array();
    Scope {
        model,
        session: session_key(req, body),
        request_fingerprint: prefix_fingerprint(&items, items.len()),
        writes: Default::default(),
    }
}

/// `codexReasoningReplaySessionKey` for a Claude request: the Claude Code agent, the
/// execution session, prompt-cache or window ids in the upstream body or the client
/// body, then the client's session headers.
fn session_key(req: &ExecRequest, body: &[u8]) -> String {
    crate::replay::claude_code_session(&req.body, &req.headers)
        .or_else(|| {
            let execution = req.execution_session.as_deref().map(str::trim).unwrap_or_default();
            (!execution.is_empty()).then(|| format!("execution:{execution}"))
        })
        .or_else(|| crate::replay::payload_session(body))
        .or_else(|| crate::replay::payload_session(&req.body))
        .or_else(|| crate::replay::header_session(&req.headers))
        .unwrap_or_default()
}

/// `applyCodexReasoningReplayCacheRequired`: cached turns inserted into the upstream
/// body. The scope is returned for caching and clearing.
pub(crate) async fn apply(cache: &Cache, req: &ExecRequest, body: String) -> (String, Scope) {
    let scope = scope(req, body.as_bytes());
    if !scope.valid() {
        return (body, scope);
    }
    // Go's HTTP executor ignores a failed read (`applyCodexReasoningReplayCache`).
    let items = match cpa_home::kv::current_client() {
        Ok(None) => cache.get(&scope.model, &scope.session),
        Ok(Some(client)) => home_get(&client, &scope.model, &scope.session).await.ok().flatten(),
        Err(_) => None,
    };
    let Some(items) = items else {
        return (body, scope);
    };
    match insert_turns(body.as_bytes(), &items).and_then(|b| String::from_utf8(b).ok()) {
        Some(updated) => (updated, scope),
        None => (body, scope),
    }
}

/// `cacheCodexReasoningReplayFromCompleted`: the turn's reasoning and tool calls from a
/// `response.completed` payload, behind a fingerprinted marker.
pub(crate) fn cache_completed(cache: &Cache, scope: &Scope, completed: &[u8]) {
    if !scope.valid() {
        return;
    }
    let output = gj::get(completed, "response.output");
    if !output.is_array() {
        return;
    }
    let mut replay: Vec<Vec<u8>> = Vec::new();
    let mut call_ids = Vec::new();
    let mut assistant = String::new();
    for item in output.array() {
        match field(&item, "type").as_str() {
            "reasoning" => replay.push(item.raw().to_vec()),
            "function_call" | "custom_tool_call" => {
                replay.push(item.raw().to_vec());
                let id = field(&item, "call_id");
                if !id.is_empty() {
                    call_ids.push(id);
                }
            }
            "message" => {
                let fingerprint = assistant_fingerprint(&item);
                if !fingerprint.is_empty() {
                    assistant = fingerprint;
                }
            }
            _ => {}
        }
    }
    if replay.is_empty() {
        return;
    }
    let mut hasher = Sha256::new();
    hasher.update(scope.request_fingerprint.as_bytes());
    hasher.update(format!("\0assistant\0{assistant}").as_bytes());
    for id in &call_ids {
        hasher.update(format!("\0call\0{id}").as_bytes());
    }
    for item in &replay {
        hasher.update(b"\0item\0");
        hasher.update(item);
    }
    let mut marker = format!(r#"{{"type":"{TURN_TYPE}"}}"#).into_bytes();
    gj::set_str(&mut marker, "id", hex(&hasher.finalize()));
    if !assistant.is_empty() {
        gj::set_str(&mut marker, "assistant_fingerprint", &assistant);
    }
    if !scope.request_fingerprint.is_empty() {
        gj::set_str(&mut marker, "request_fingerprint", &scope.request_fingerprint);
    }
    for id in &call_ids {
        gj::set_str(&mut marker, "call_ids.-1", id);
    }
    let mut items = Vec::with_capacity(replay.len() + 1);
    items.push(marker);
    items.extend(replay);
    match cpa_home::kv::current_client() {
        Ok(None) => {
            cache.append(&scope.model, &scope.session, &items);
        }
        Ok(Some(client)) => {
            let (model, session) = (scope.model.clone(), scope.session.clone());
            scope.writes.spawn(async move {
                home_append(&client, &model, &session, &items).await;
            });
        }
        Err(error) => {
            tracing::error!("home kv best-effort codex reasoning replay append failed prefix=cpa:codex:*: {error}");
        }
    }
}

/// `clearCodexReasoningReplayOnInvalidSignature`.
pub(crate) fn clear_on_invalid_signature(cache: &Cache, scope: &Scope, status: u16, body: &[u8]) {
    if scope.valid()
        && crate::codex_response::classification(status, &String::from_utf8_lossy(body))
            .is_some_and(|(code, _)| code == "thinking_signature_invalid")
    {
        match cpa_home::kv::current_client() {
            Ok(None) => cache.delete(&scope.model, &scope.session),
            Ok(Some(client)) => {
                let (model, session) = (scope.model.clone(), scope.session.clone());
                scope.writes.spawn(async move {
                    if let Err(error) = home_delete(&client, &model, &session).await {
                        tracing::warn!("codex reasoning replay cache delete failed: {error}");
                    }
                });
            }
            Err(error) => tracing::warn!("codex reasoning replay cache delete failed: {error}"),
        }
    }
}

// --------------------------------------------------------------------------- insertion

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// `codexReplayInputPrefixFingerprint`.
fn prefix_fingerprint(items: &[Res<'_>], end: usize) -> String {
    let mut hasher = Sha256::new();
    for item in items.iter().take(end) {
        hasher.update(b"\0item\0");
        hasher.update(item.raw());
    }
    hex(&hasher.finalize())
}

/// `codexReplayPrefixFingerprints`: every prefix fingerprint from one hashing pass,
/// extended on demand.
struct Prefixes<'r, 'a> {
    items: &'r [Res<'a>],
    hasher: Sha256,
    sums: Vec<String>,
}

impl<'r, 'a> Prefixes<'r, 'a> {
    fn new(items: &'r [Res<'a>]) -> Self {
        let hasher = Sha256::new();
        let first = hex(&hasher.clone().finalize());
        Self {
            items,
            hasher,
            sums: vec![first],
        }
    }

    fn at(&mut self, end: usize) -> &str {
        while self.sums.len() <= end {
            let next = self.sums.len() - 1;
            self.hasher.update(b"\0item\0");
            self.hasher.update(self.items[next].raw());
            self.sums.push(hex(&self.hasher.clone().finalize()));
        }
        &self.sums[end]
    }
}

#[derive(Default)]
struct Turn {
    marked: bool,
    assistant_fingerprint: String,
    request_fingerprint: String,
    call_ids: Vec<String>,
    items: Vec<Vec<u8>>,
}

/// `splitCodexReasoningReplayTurns`.
fn split_turns(items: &[Vec<u8>]) -> Vec<Turn> {
    let mut turns = Vec::new();
    let mut current = Turn::default();
    for item in items {
        let r = gj::parse(item);
        if field(&r, "type") == TURN_TYPE {
            if !current.items.is_empty() {
                turns.push(std::mem::take(&mut current));
            }
            current = Turn {
                marked: true,
                assistant_fingerprint: field(&r, "assistant_fingerprint"),
                request_fingerprint: field(&r, "request_fingerprint"),
                ..Turn::default()
            };
            let ids = r.get("call_ids");
            if ids.is_array() {
                current.call_ids = ids.array().iter().map(text).filter(|s| !s.is_empty()).collect();
            }
            continue;
        }
        current.items.push(item.clone());
    }
    if !current.items.is_empty() {
        turns.push(current);
    }
    turns
}

/// `insertCodexReasoningReplayTurns`.
fn insert_turns(body: &[u8], replay: &[Vec<u8>]) -> Option<Vec<u8>> {
    let input = gj::get(body, "input");
    if !input.is_array() || replay.is_empty() {
        return None;
    }
    let inputs = input.array();
    let mut insertions: HashMap<usize, Vec<Vec<u8>>> = HashMap::new();
    let mut used: HashSet<usize> = HashSet::new();
    let mut prefixes = Prefixes::new(&inputs);
    let mut fallback_end = inputs.len() as isize - 1;
    let mut inserted = false;
    for turn in split_turns(replay).iter().rev() {
        if turn.items.is_empty() {
            continue;
        }
        let (index, items) = if turn.marked {
            let Some(anchor) = anchor_index(&inputs, turn, fallback_end, &used, &mut prefixes) else {
                continue;
            };
            used.insert(anchor);
            if turn.request_fingerprint.is_empty() {
                fallback_end = anchor as isize - 1;
            }
            (anchor, filter_turn_items(&inputs, &turn.items))
        } else {
            let items = filter_items_for_input(&inputs, &turn.items);
            if items.is_empty() {
                continue;
            }
            (insert_index(&inputs, &items), items)
        };
        if items.is_empty() {
            continue;
        }
        let mut items = align_call_ids(&inputs, items);
        let slot = insertions.entry(index).or_default();
        items.append(slot);
        *slot = items;
        inserted = true;
    }
    if !inserted {
        return None;
    }
    let mut out: Vec<&[u8]> = Vec::with_capacity(inputs.len() + replay.len());
    for (index, item) in inputs.iter().enumerate() {
        out.extend(insertions.get(&index).into_iter().flatten().map(Vec::as_slice));
        out.push(item.raw());
    }
    out.extend(insertions.get(&inputs.len()).into_iter().flatten().map(Vec::as_slice));
    let mut array = b"[".to_vec();
    array.extend(out.join(&b","[..]));
    array.push(b']');
    gj::try_set_raw(body, "input", array).ok()
}

fn is_tool_call(kind: &str) -> bool {
    matches!(kind, "function_call" | "custom_tool_call")
}

fn is_tool_output(kind: &str) -> bool {
    matches!(kind, "function_call_output" | "custom_tool_call_output")
}

/// `codexReasoningReplayTurnAnchorIndex`.
fn anchor_index(
    inputs: &[Res<'_>],
    turn: &Turn,
    fallback_end: isize,
    used: &HashSet<usize>,
    prefixes: &mut Prefixes<'_, '_>,
) -> Option<usize> {
    let mut search_end = if turn.request_fingerprint.is_empty() {
        fallback_end
    } else {
        inputs.len() as isize - 1
    };
    search_end = search_end.min(inputs.len() as isize - 1);
    let candidates = || (0..=search_end).rev().map(|i| i as usize);
    let matches_prefix = |index: usize, prefixes: &mut Prefixes<'_, '_>| {
        turn.request_fingerprint.is_empty() || prefixes.at(index) == turn.request_fingerprint
    };
    if !turn.call_ids.is_empty() {
        let ids: HashSet<String> = turn.call_ids.iter().flat_map(|id| comparable_call_ids(id)).collect();
        for index in candidates() {
            if used.contains(&index) || !matches_prefix(index, prefixes) {
                continue;
            }
            let kind = field(&inputs[index], "type");
            if !is_tool_call(&kind) && !is_tool_output(&kind) {
                continue;
            }
            if comparable_call_ids(&inputs[index].get("call_id").str())
                .iter()
                .any(|c| ids.contains(c))
            {
                return Some(index);
            }
        }
    }
    if !turn.assistant_fingerprint.is_empty() {
        for index in candidates() {
            if used.contains(&index) || !matches_prefix(index, prefixes) {
                continue;
            }
            if assistant_fingerprint(&inputs[index]) == turn.assistant_fingerprint {
                return Some(index);
            }
        }
    }
    if turn.call_ids.is_empty() && turn.assistant_fingerprint.is_empty() {
        return Some(insert_index(inputs, &turn.items));
    }
    None
}

/// `filterCodexReasoningReplayTurnItems`: skips reasoning already in the input and
/// tool calls the input already has or never answers.
fn filter_turn_items(inputs: &[Res<'_>], items: &[Vec<u8>]) -> Vec<Vec<u8>> {
    let mut reasoning = HashSet::new();
    let mut calls = HashSet::new();
    let mut outputs = HashSet::new();
    for input in inputs {
        let kind = field(input, "type");
        if kind == "reasoning" {
            let encrypted = field(input, "encrypted_content");
            if !encrypted.is_empty() {
                reasoning.insert(encrypted);
            }
        } else if is_tool_output(&kind) {
            outputs.extend(comparable_call_ids(&input.get("call_id").str()));
        }
        calls.extend(tool_call_keys(input));
    }
    let mut filtered = Vec::new();
    for item in items {
        let r = gj::parse(item);
        match field(&r, "type").as_str() {
            "reasoning" if reasoning.contains(&field(&r, "encrypted_content")) => continue,
            "reasoning" => {}
            kind if is_tool_call(kind) => {
                if !keep_call(&r, &mut calls, &outputs) {
                    continue;
                }
            }
            _ => continue,
        }
        filtered.push(item.clone());
    }
    filtered
}

/// A replayed tool call is kept when it is new and the input holds its output.
fn keep_call(call: &Res<'_>, calls: &mut HashSet<String>, outputs: &HashSet<String>) -> bool {
    let keys = tool_call_keys(call);
    if keys.is_empty() || keys.iter().any(|k| calls.contains(k)) {
        return false;
    }
    if !comparable_call_ids(&call.get("call_id").str())
        .iter()
        .any(|c| outputs.contains(c))
    {
        return false;
    }
    calls.extend(keys);
    true
}

/// `filterCodexReasoningReplayItemsForInput` for unmarked (legacy) entries.
fn filter_items_for_input(inputs: &[Res<'_>], items: &[Vec<u8>]) -> Vec<Vec<u8>> {
    let has_reasoning = inputs.iter().any(|item| {
        let encrypted = item.get("encrypted_content");
        field(item, "type") == "reasoning"
            && encrypted.kind == Kind::String
            && cpa_common::signature::is_valid_gpt_reasoning_signature(&encrypted.bytes()[..])
    });
    let mut calls = HashSet::new();
    let mut outputs = HashSet::new();
    for input in inputs {
        if is_tool_output(&field(input, "type")) {
            let id = field(input, "call_id");
            if !id.is_empty() {
                outputs.extend(comparable_call_ids(&id));
            }
        }
        calls.extend(tool_call_keys(input));
    }
    let mut filtered = Vec::new();
    for item in items {
        let r = gj::parse(item);
        match field(&r, "type").as_str() {
            "reasoning" if has_reasoning => continue,
            "reasoning" => {}
            kind if is_tool_call(kind) => {
                if !keep_call(&r, &mut calls, &outputs) {
                    continue;
                }
            }
            _ => continue,
        }
        filtered.push(item.clone());
    }
    filtered
}

/// `codexReplayAssistantMessageFingerprint`: the SHA-256 of an assistant message's text.
fn assistant_fingerprint(item: &Res<'_>) -> String {
    let kind = field(item, "type");
    if !kind.is_empty() && kind != "message" {
        return String::new();
    }
    if !field(item, "role").eq_ignore_ascii_case("assistant") {
        return String::new();
    }
    let content = item.get("content");
    let mut text_bytes = Vec::new();
    if content.kind == Kind::String {
        text_bytes.extend_from_slice(&content.bytes());
    } else if content.is_array() {
        for part in content.array() {
            match field(&part, "type").as_str() {
                "input_text" | "output_text" => text_bytes.extend_from_slice(&part.get("text").bytes()),
                "refusal" => {
                    text_bytes.extend_from_slice(b"\0refusal\0");
                    text_bytes.extend_from_slice(&part.get("refusal").bytes());
                }
                _ => return String::new(),
            }
        }
    } else {
        return String::new();
    }
    if text_bytes.is_empty() {
        return String::new();
    }
    hex(&Sha256::digest(&text_bytes))
}

/// `codexReasoningReplayInsertIndex`: before the output answering a replayed call, else
/// at the last assistant message, else before the first non-instruction item.
fn insert_index(inputs: &[Res<'_>], replay: &[Vec<u8>]) -> usize {
    let mut ids = HashSet::new();
    for item in replay {
        let r = gj::parse(item);
        if is_tool_call(&field(&r, "type")) {
            ids.extend(comparable_call_ids(&r.get("call_id").str()));
        }
    }
    if !ids.is_empty() {
        for (index, input) in inputs.iter().enumerate() {
            if !is_tool_output(&field(input, "type")) {
                continue;
            }
            let id = field(input, "call_id");
            if id.is_empty() || ids.contains(&id) {
                return index;
            }
        }
    }
    if let Some(index) = inputs
        .iter()
        .rposition(|i| message_role(i).as_deref() == Some("assistant"))
    {
        return index;
    }
    inputs
        .iter()
        .position(|i| !matches!(message_role(i).as_deref(), Some("developer" | "system")))
        .unwrap_or(inputs.len())
}

/// `codexAlignReasoningReplayToolCallIDs`: replayed calls take the call id the input's
/// output uses.
fn align_call_ids(inputs: &[Res<'_>], items: Vec<Vec<u8>>) -> Vec<Vec<u8>> {
    let mut output_ids: HashMap<String, String> = HashMap::new();
    for input in inputs {
        if !is_tool_output(&field(input, "type")) {
            continue;
        }
        let id = field(input, "call_id");
        if id.is_empty() {
            continue;
        }
        for candidate in comparable_call_ids(&id) {
            output_ids.insert(candidate, id.clone());
        }
    }
    if output_ids.is_empty() {
        return items;
    }
    items
        .into_iter()
        .map(|item| {
            if !is_tool_call(&item_type(&item)) {
                return item;
            }
            let id = text(&gj::get(&item, "call_id"));
            let output = comparable_call_ids(&id)
                .into_iter()
                .find_map(|c| output_ids.get(&c).filter(|v| !v.is_empty()).cloned());
            match output {
                Some(output) if output != id => gj::try_set_str(&item, "call_id", &output).unwrap_or(item),
                _ => item,
            }
        })
        .collect()
}

/// `codexReplayMessageRole`.
fn message_role(item: &Res<'_>) -> Option<String> {
    let kind = field(item, "type");
    let role = field(item, "role").to_lowercase();
    (!role.is_empty() && (kind.is_empty() || kind == "message")).then_some(role)
}

/// `codexReplayToolCallKeys`.
fn tool_call_keys(item: &Res<'_>) -> Vec<String> {
    let kind = field(item, "type");
    if !is_tool_call(&kind) {
        return Vec::new();
    }
    comparable_call_ids(&item.get("call_id").str())
        .into_iter()
        .map(|id| format!("{kind}:{id}"))
        .collect()
}

/// `codexReplayComparableCallIDs`: the id, plus the form Claude clients see
/// (`SanitizeClaudeToolID`, shortened to 64 bytes) when it differs.
fn comparable_call_ids(id: &str) -> Vec<String> {
    let id = id.trim();
    if id.is_empty() {
        return Vec::new();
    }
    let sanitized: String = id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect();
    let visible = shorten(sanitized);
    if visible.is_empty() || visible == id {
        vec![id.to_owned()]
    } else {
        vec![id.to_owned(), visible]
    }
}

/// `shortenCodexReplayCallIDIfNeeded`.
fn shorten(id: String) -> String {
    const LIMIT: usize = 64;
    if id.len() <= LIMIT {
        return id;
    }
    let sum = Sha256::digest(id.as_bytes());
    let suffix = format!("_{}", hex(&sum[..8]));
    format!("{}{suffix}", &id[..LIMIT - suffix.len()])
}

#[cfg(test)]
#[path = "codex_replay_tests.rs"]
mod tests;
