//! Tool-call repair for Responses WebSocket turns served over HTTP
//! (openai_responses_websocket_toolcall_repair.go).
//!
//! Upstream rejects transcripts with a tool call but no output, or an output with no
//! call. Turns that are rebuilt from local state drop such orphans or restore the missing
//! half from what earlier turns of the same downstream session (`x-client-request-id`,
//! turn-metadata `session_id`, `session-id`) committed. The caches are shared by every
//! connection with that session key and dropped when the last one closes. Each session
//! keeps at most 256 calls and 256 outputs, so memory stays bounded per session.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::{LazyLock, Mutex, PoisonError};

use axum::http::HeaderMap;
use gjson::Kind;

use crate::websocket_requests::{Item, dedupe_ids, is_complete_tool_call, is_tool_call, is_tool_output, marshal};

/// `websocketToolOutputCacheMaxPerSession`.
const MAX_PER_SESSION: usize = 256;

#[derive(Default)]
struct Entries {
    items: HashMap<String, String>,
    order: VecDeque<String>,
}

impl Entries {
    fn record(&mut self, call_id: &str, raw: &str) {
        if self.items.insert(call_id.to_owned(), raw.to_owned()).is_none() {
            self.order.push_back(call_id.to_owned());
        }
        while self.order.len() > MAX_PER_SESSION {
            if let Some(evicted) = self.order.pop_front() {
                self.items.remove(&evicted);
            }
        }
    }
}

#[derive(Default)]
struct Caches {
    outputs: HashMap<String, Entries>,
    calls: HashMap<String, Entries>,
    refs: HashMap<String, usize>,
}

static CACHES: LazyLock<Mutex<Caches>> = LazyLock::new(Mutex::default);

fn caches() -> std::sync::MutexGuard<'static, Caches> {
    CACHES.lock().unwrap_or_else(PoisonError::into_inner)
}

fn header<'a>(headers: &'a HeaderMap, name: &str) -> &'a str {
    headers
        .get(name)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .trim()
}

/// `websocketDownstreamSessionKey`.
pub(crate) fn session_key(headers: &HeaderMap) -> String {
    let request_id = header(headers, "x-client-request-id");
    if !request_id.is_empty() {
        return request_id.to_owned();
    }
    let metadata = header(headers, "x-codex-turn-metadata");
    if !metadata.is_empty() {
        let id = gjson::get(metadata, "session_id");
        let id = id.str().trim();
        if !id.is_empty() {
            return id.to_owned();
        }
    }
    for name in ["session-id", "session_id"] {
        let id = header(headers, name);
        if !id.is_empty() {
            return id.to_owned();
        }
    }
    String::new()
}

/// Whether an open downstream connection holds `key`'s caches.
#[cfg(test)]
pub(crate) fn is_retained(key: &str) -> bool {
    caches().refs.contains_key(key)
}

/// Holds the session's caches while a downstream connection is open
/// (`retainResponsesWebsocketToolCaches` / `releaseResponsesWebsocketToolCaches`).
pub(crate) struct Retained(String);

impl Retained {
    pub fn new(key: String) -> Self {
        if !key.is_empty() {
            *caches().refs.entry(key.clone()).or_default() += 1;
        }
        Self(key)
    }
}

impl Drop for Retained {
    fn drop(&mut self) {
        if self.0.is_empty() {
            return;
        }
        let mut caches = caches();
        let count = caches.refs.get(&self.0).copied().unwrap_or_default();
        if count > 1 {
            caches.refs.insert(self.0.clone(), count - 1);
            return;
        }
        caches.refs.remove(&self.0);
        caches.outputs.remove(&self.0);
        caches.calls.remove(&self.0);
    }
}

/// Tool calls and outputs seen in one turn, committed only when the turn succeeds
/// (`responsesWebsocketToolCacheTurn`).
pub(crate) struct TurnCache {
    key: String,
    outputs: Vec<(String, String)>,
    calls: Vec<(String, String)>,
}

impl TurnCache {
    fn record(&mut self, kind: &str, call_id: &str, raw: &str) {
        let call_id = call_id.trim();
        if call_id.is_empty() || raw.trim().is_empty() {
            return;
        }
        let list = if is_tool_output(kind) {
            &mut self.outputs
        } else if is_tool_call(kind) {
            &mut self.calls
        } else {
            return;
        };
        match list.iter_mut().find(|(id, _)| id == call_id) {
            Some(entry) => entry.1 = raw.to_owned(),
            None => list.push((call_id.to_owned(), raw.to_owned())),
        }
    }

    /// `recordResponse`: complete tool calls the upstream produced.
    pub fn record_response(&mut self, payload: &str) {
        for item in response_tool_calls(payload) {
            self.record(&item.0, &item.1, &item.2);
        }
    }

    pub fn commit(self) {
        let mut caches = caches();
        let outputs = caches.outputs.entry(self.key.clone()).or_default();
        for (id, raw) in &self.outputs {
            outputs.record(id, raw);
        }
        let calls = caches.calls.entry(self.key).or_default();
        for (id, raw) in &self.calls {
            calls.record(id, raw);
        }
    }
}

/// Complete tool calls in a `response.completed` output or an `output_item` event, as
/// `(type, call_id, raw item)`.
fn response_tool_calls(payload: &str) -> Vec<(String, String, String)> {
    let items = match gjson::get(payload, "type").str().trim() {
        "response.completed" => {
            let output = gjson::get(payload, "response.output");
            if output.kind() != Kind::Array {
                return Vec::new();
            }
            output.array().iter().map(|v| v.json().to_owned()).collect()
        }
        "response.output_item.added" | "response.output_item.done" => {
            vec![gjson::get(payload, "item").json().to_owned()]
        }
        _ => return Vec::new(),
    };
    items
        .into_iter()
        .filter_map(|raw| {
            let item = gjson::parse(&raw);
            is_complete_tool_call(&item).then(|| {
                (
                    item.get("type").str().trim().to_owned(),
                    item.get("call_id").str().trim().to_owned(),
                    raw.clone(),
                )
            })
        })
        .collect()
}

/// `recordResponsesWebsocketToolCallsFromPayload`: turns on the upstream socket record
/// the calls they forward straight into the session cache.
pub(crate) fn record_calls(key: &str, payload: &str) {
    if key.is_empty() {
        return;
    }
    let found = response_tool_calls(payload);
    if found.is_empty() {
        return;
    }
    let mut caches = caches();
    let calls = caches.calls.entry(key.to_owned()).or_default();
    for (_, id, raw) in found {
        calls.record(&id, &raw);
    }
}

/// `prepareResponsesWebsocketFallbackTurn`: repairs `payload` against the committed
/// caches and returns the turn recorder (none without a session key).
pub(crate) fn prepare_fallback_turn(key: &str, payload: String) -> (String, Option<TurnCache>) {
    let mut turn = (!key.is_empty()).then(|| TurnCache {
        key: key.to_owned(),
        outputs: Vec::new(),
        calls: Vec::new(),
    });
    let repaired = repair(key, &payload, turn.as_mut()).unwrap_or(payload);
    (repaired, turn)
}

/// `responsesWebsocketMetadataString` of the raw `previous_response_id`.
fn metadata_string(raw: &str) -> String {
    let raw = raw.trim();
    if raw.is_empty() || raw == "null" {
        return String::new();
    }
    if raw.starts_with('"') {
        return gjson::parse(raw).str().trim().to_owned();
    }
    raw.to_owned()
}

/// `repairResponsesWebsocketToolCallsWithCachesMode` without recording. `None` when the
/// payload stays unchanged.
fn repair(key: &str, payload: &str, turn: Option<&mut TurnCache>) -> Option<String> {
    if !gjson::valid(payload) {
        return None;
    }
    let root = gjson::parse(payload);
    if root.kind() != Kind::Object {
        return None;
    }
    let (mut input, mut previous, mut valid) = (None::<String>, String::new(), true);
    root.each(|k, v| {
        let key = k.str();
        if key.eq_ignore_ascii_case("input") {
            if v.kind() != Kind::Array && v.json().trim() != "null" {
                valid = false;
                return false;
            }
            input = (v.kind() == Kind::Array).then(|| v.json().to_owned());
        } else if key.eq_ignore_ascii_case("previous_response_id") {
            previous = v.json().to_owned();
        }
        true
    });
    let input = input.filter(|_| valid)?;
    let items: Vec<Item> = crate::websocket_requests::parse_items(&gjson::parse(&input));
    let enabled = !key.is_empty();
    let updated = if enabled {
        let caches = caches();
        repair_items(&caches, key, &items, !metadata_string(&previous).is_empty(), turn)
    } else {
        dedupe_ids(items.clone())
    };
    if updated.len() == items.len() && updated.iter().zip(&items).all(|(a, b)| a.raw == b.raw) {
        return None;
    }
    // ponytail: Go splices the last case-insensitive `input` member; this edits `input`.
    Some(crate::websocket_requests::set_raw(payload, "input", &marshal(&updated)))
}

/// `repairResponsesToolCallItems` with repair enabled.
fn repair_items(
    caches: &Caches,
    key: &str,
    items: &[Item],
    allow_orphan_outputs: bool,
    mut turn: Option<&mut TurnCache>,
) -> Vec<Item> {
    let mut output_present = HashSet::new();
    let mut call_present = HashSet::new();
    for item in items {
        if let Some(turn) = turn.as_deref_mut() {
            turn.record(&item.kind, &item.call_id, &item.raw);
        }
        if item.call_id.is_empty() {
            continue;
        }
        if is_tool_output(&item.kind) {
            output_present.insert(item.call_id.clone());
        } else if is_tool_call(&item.kind) {
            call_present.insert(item.call_id.clone());
        }
    }
    let cached = |map: &HashMap<String, Entries>, id: &str| map.get(key).and_then(|e| e.items.get(id)).cloned();
    let mut filtered = Vec::with_capacity(items.len());
    let mut inserted = HashSet::new();
    for item in items {
        if is_tool_output(&item.kind) {
            if item.call_id.is_empty() {
                // Codex sends standalone named results (heartbeats, delegation input).
                let name = gjson::get(&item.raw, "name");
                if item.kind == "function_call_output" && name.kind() == Kind::String && !name.str().trim().is_empty() {
                    filtered.push(item.clone());
                }
                continue;
            }
            if call_present.contains(&item.call_id) || allow_orphan_outputs {
                filtered.push(item.clone());
                continue;
            }
            if let Some(raw) = cached(&caches.calls, &item.call_id) {
                if inserted.insert(item.call_id.clone()) {
                    filtered.push(Item::parse(&gjson::parse(&raw)));
                    call_present.insert(item.call_id.clone());
                }
                filtered.push(item.clone());
            }
            continue;
        }
        if !is_tool_call(&item.kind) {
            filtered.push(item.clone());
            continue;
        }
        if item.call_id.is_empty() {
            continue;
        }
        if output_present.contains(&item.call_id) || allow_orphan_outputs {
            filtered.push(item.clone());
            continue;
        }
        if let Some(raw) = cached(&caches.outputs, &item.call_id) {
            filtered.push(item.clone());
            filtered.push(Item::parse(&gjson::parse(&raw)));
            output_present.insert(item.call_id.clone());
        }
    }
    dedupe_ids(filtered)
}

// Repair results are checked against Go in websocket_requests_tests.rs.
#[cfg(test)]
mod tests {
    use super::*;

    fn headers(pairs: &[(&'static str, &str)]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for (k, v) in pairs {
            h.insert(*k, v.parse().unwrap());
        }
        h
    }

    #[test]
    fn session_key_follows_go_priority() {
        assert_eq!(
            session_key(&headers(&[("x-client-request-id", " req "), ("session-id", "s")])),
            "req"
        );
        assert_eq!(
            session_key(&headers(&[
                ("x-codex-turn-metadata", r#"{"session_id":"meta"}"#),
                ("session_id", "s")
            ])),
            "meta"
        );
        assert_eq!(session_key(&headers(&[("session_id", "under")])), "under");
        assert_eq!(session_key(&HeaderMap::new()), "");
    }

    #[test]
    fn release_of_last_connection_drops_session_caches() {
        let key = "tools-test-release";
        let first = Retained::new(key.into());
        let second = Retained::new(key.into());
        record_calls(
            key,
            r#"{"type":"response.output_item.done","item":{"type":"function_call","call_id":"c9","name":"f","arguments":"{}"}}"#,
        );
        drop(first);
        assert!(caches().calls.contains_key(key));
        drop(second);
        assert!(!caches().calls.contains_key(key));
    }

    #[test]
    fn entries_evict_oldest_beyond_cap() {
        let mut entries = Entries::default();
        for i in 0..=MAX_PER_SESSION {
            entries.record(&format!("c{i}"), "{}");
        }
        assert_eq!(entries.items.len(), MAX_PER_SESSION);
        assert!(!entries.items.contains_key("c0"));
        assert!(entries.items.contains_key(&format!("c{MAX_PER_SESSION}")));
    }
}
