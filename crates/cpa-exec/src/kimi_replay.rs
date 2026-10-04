//! Kimi native reasoning replay for Claude-format requests (kimi_thinking_replay.go,
//! internal/cache/kimi_thinking_replay_cache.go): Kimi's cache, model family and request
//! preparation on top of the shared replay helpers (crate::replay).
//!
//! After a complete response with signed thinking and a tool call, the assistant content
//! is cached per (model family, session); the next request restores it into the matching
//! assistant turn. Writes are conditional on the generation read, so a slower request
//! never overwrites newer content.
//!
//! In Home mode the state lives in Home KV (`cpa:kimi:thinking-replay:*`, crate::home_replay)
//! as Go writes it. Cache failures only skip replay, as in Go.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use bytes::Bytes;
use cpa_common::json as gj;
use cpa_core::exec::{ExecError, ExecRequest};

use crate::replay::{MAX_BLOCKS_PER_ENTRY, MAX_BYTES_PER_ENTRY, ReplayTarget, replayable, restore_turn, session_key};

const TTL: Duration = Duration::from_secs(3600);
const MAX_ENTRIES: usize = 10240;
const EVICT_BATCH: usize = 128;
const MAX_TOTAL_BYTES: usize = 256 << 20;
/// `kimiThinkingReplayCacheMaxSerializedBytes`.
const MAX_SERIALIZED_BYTES: usize = MAX_BYTES_PER_ENTRY + 1024;

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

/// Where one request's snapshot came from.
enum Backend {
    Local {
        cache: Arc<ReplayCache>,
        snapshot: Snapshot,
    },
    Home {
        client: cpa_home::Client,
        key: String,
        snapshot: crate::home_replay::Snapshot,
    },
}

/// One request's replay scope; `backend` is set once the cache read succeeded (Go
/// `cacheReady`).
pub(crate) struct Scope {
    key: String,
    backend: Option<Backend>,
    pub(crate) applied: bool,
    /// Home writes the response waits for.
    pub(crate) writes: crate::home_replay::Writes,
}

impl Scope {
    /// Caches complete replayable content, or clears stale content otherwise.
    pub(crate) fn store(&self, content: &[u8]) {
        if replayable(content) {
            self.write(Some(content));
        } else {
            self.write(None);
        }
    }

    pub(crate) fn clear(&self) {
        self.write(None);
    }

    /// Replaces (`Some`) or deletes (`None`) the state this request read, only if it is
    /// still current.
    fn write(&self, content: Option<&[u8]>) {
        if self.key.is_empty() {
            return;
        }
        match &self.backend {
            None => {}
            Some(Backend::Local { cache, snapshot }) => match content {
                Some(content) => {
                    cache.replace_if_unchanged(&self.key, *snapshot, content);
                }
                None => {
                    cache.delete_if_unchanged(&self.key, *snapshot);
                }
            },
            Some(Backend::Home { client, key, snapshot }) => {
                let (client, key, snapshot) = (client.clone(), key.clone(), snapshot.clone());
                let content = content.map(<[u8]>::to_vec);
                self.writes.spawn(async move {
                    let written = match &content {
                        Some(content) => home_replace(&client, &key, &snapshot, content).await,
                        None => home_delete(&client, &key, &snapshot).await,
                    };
                    if let Err(error) = written {
                        let what = if content.is_some() { "replace" } else { "delete" };
                        tracing::warn!("kimi thinking replay cache {what} failed: {error}");
                    }
                });
            }
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
pub(crate) async fn prepare(cache: &Arc<ReplayCache>, req: &mut ExecRequest) -> Scope {
    let family = model_family(&req.model);
    let session = session_key(req, &req.body);
    let key = if family.trim().is_empty() || session.trim().is_empty() {
        String::new()
    } else {
        format!("kimi-thinking-replay\0{}\0{}", family.trim(), session.trim())
    };
    let mut scope = Scope {
        key,
        backend: None,
        applied: false,
        writes: Default::default(),
    };
    if scope.key.is_empty() {
        return scope;
    }
    let content = match cpa_home::kv::current_client() {
        Ok(None) => {
            let (content, snapshot) = cache.get(&scope.key, Instant::now());
            scope.backend = Some(Backend::Local {
                cache: cache.clone(),
                snapshot,
            });
            content.map(|c| c.to_vec())
        }
        Ok(Some(client)) => {
            let key = kv_key(&family, &session);
            match home_get(&client, &key).await {
                Ok((snapshot, content)) => {
                    scope.backend = Some(Backend::Home { client, key, snapshot });
                    content
                }
                Err(error) => {
                    tracing::warn!("kimi thinking replay cache read failed: {error}");
                    None
                }
            }
        }
        Err(error) => {
            tracing::warn!("kimi thinking replay cache read failed: {error}");
            None
        }
    };
    if let Some(content) = content
        && let Some(updated) = restore_turn(&req.body, &content)
    {
        req.body = Bytes::from(updated);
        scope.applied = true;
    }
    scope
}

/// `kimiThinkingReplayKVKey`.
fn kv_key(family: &str, session: &str) -> String {
    format!(
        "cpa:kimi:thinking-replay:{}:{}",
        cpa_home::kv::hash_key_part(family.trim()),
        cpa_home::kv::hash_key_part(session.trim())
    )
}

/// `decodeKimiThinkingReplayHomeValue`: `(content, deleted)`, or `None` when invalid. A
/// bare content array is a legacy value.
fn decode(raw: &[u8]) -> Option<(Option<Vec<u8>>, bool)> {
    if raw.is_empty() || raw.len() > MAX_SERIALIZED_BYTES || !gj::valid(raw) {
        return None;
    }
    if gj::parse(raw).is_array() {
        return valid_content(raw).then(|| (Some(raw.to_vec()), false));
    }
    let generation = gj::get(raw, "generation");
    if generation.kind != gj::Kind::String || generation.str().trim().is_empty() {
        return None;
    }
    if gj::get(raw, "deleted").bool() {
        return Some((None, true));
    }
    let content = gj::get(raw, "content");
    let content = content.raw();
    valid_content(content).then(|| (Some(content.to_vec()), false))
}

/// Go `GetKimiThinkingReplayWithSnapshotRequired` in Home mode: the snapshot and the
/// content it holds (none for a tombstone). A hit renews the TTL.
async fn home_get(
    client: &cpa_home::Client,
    key: &str,
) -> Result<(crate::home_replay::Snapshot, Option<Vec<u8>>), String> {
    let snapshot =
        crate::home_replay::read_or_reserve(client, key, MAX_SERIALIZED_BYTES, "kimi thinking replay").await?;
    let (content, _deleted) = decode(&snapshot.raw).ok_or("invalid kimi thinking replay content")?;
    if let Err(error) = client.kv_expire(key, crate::home_replay::TTL).await {
        tracing::warn!("home kv kimi thinking replay expire failed prefix=cpa:kimi:*: {error}");
    }
    Ok((snapshot, content))
}

/// Go `ReplaceKimiThinkingReplayIfUnchanged` in Home mode.
async fn home_replace(
    client: &cpa_home::Client,
    key: &str,
    snapshot: &crate::home_replay::Snapshot,
    content: &[u8],
) -> Result<bool, String> {
    if !valid_content(content) {
        return Ok(false);
    }
    let value = crate::home_replay::envelope(
        &crate::home_replay::generation(),
        Some(crate::home_replay::Payload::Content(content)),
    );
    crate::home_replay::swap(client, key, snapshot, &value).await
}

/// Go `DeleteKimiThinkingReplayIfUnchanged` in Home mode: a tombstone.
async fn home_delete(
    client: &cpa_home::Client,
    key: &str,
    snapshot: &crate::home_replay::Snapshot,
) -> Result<bool, String> {
    let tombstone = crate::home_replay::envelope(&crate::home_replay::generation(), None);
    crate::home_replay::swap(client, key, snapshot, &tombstone).await
}

/// Go clears applied replay only for request rejections (status 400 and 422).
pub(crate) fn clears_after(error: &ExecError) -> bool {
    matches!(error.status, 400 | 422)
}

impl ReplayTarget for Scope {
    fn store(&self, content: &[u8]) {
        Scope::store(self, content);
    }

    fn clear(&self) {
        Scope::clear(self);
    }

    fn applied(&self) -> bool {
        self.applied
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    const CACHED: &str = r#"[{"type":"thinking","thinking":"plan","signature":"sig-1"},{"type":"text","text":"Calling."},{"type":"tool_use","id":"toolu_1","name":"read","input":{"path":"a"}}]"#;

    #[test]
    fn family_shares_k3_variants_only() {
        assert_eq!(model_family("kimi-k3"), "k3");
        assert_eq!(model_family("kimi-k3-256k(high)"), "k3");
        assert_eq!(model_family("kimi-k2.8"), "kimi-for-coding");
        assert_eq!(model_family("kimi-k2.5"), "k2.5");
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
    /// Random generations read as `<gen>`, as in the Go golden.
    pub(crate) fn normalize_generations(text: &str) -> String {
        let bytes = text.as_bytes();
        let shape = |i: usize| {
            text.len() >= i + 36
                && bytes[i..i + 36].iter().enumerate().all(|(j, b)| match j {
                    8 | 13 | 18 | 23 => *b == b'-',
                    _ => b.is_ascii_digit() || (b'a'..=b'f').contains(b),
                })
        };
        let (mut out, mut i) = (String::new(), 0);
        while i < text.len() {
            if shape(i) {
                out.push_str("<gen>");
                i += 36;
            } else {
                let c = text[i..].chars().next().unwrap();
                out.push(c);
                i += c.len_utf8();
            }
        }
        out
    }

    /// Go's Home KV backend for Kimi replay, recorded by the reference's
    /// internal/cache/zz_rustgolden_test.go: reservation, CAS writes against the read
    /// bytes, tombstones, legacy arrays and invalid values.
    #[tokio::test]
    async fn home_kv_replay_matches_go() {
        let golden: serde_json::Value =
            serde_json::from_str(include_str!("claude/testdata/go_kimi_replay_home.json")).unwrap();
        let steps = golden["steps"].as_array().unwrap();
        let values = Arc::new(Mutex::new(HashMap::new()));
        let home = cpa_home::fake::FakeHome::start(crate::claude::kv_test::kv_home(values.clone())).await;
        let client = home.client();
        let content = br#"[ {"type":"thinking","thinking":"a<b & c","signature":"sig"}, {"type":"tool_use","id":"t1","name":"f","input":{}} ]"#;
        let mut seen = 0;
        let mut calls = || {
            let all: Vec<serde_json::Value> = home
                .commands()
                .iter()
                .filter_map(|c| crate::claude::kv_test::as_go_call(c))
                .map(|call| serde_json::from_str(&normalize_generations(&call.to_string())).unwrap())
                .collect();
            let new = all[seen..].to_vec();
            seen = all.len();
            serde_json::Value::from(new)
        };
        let text = |c: &Option<Vec<u8>>| String::from_utf8(c.clone().unwrap_or_default()).unwrap();
        let key = kv_key(" k3 ", " sess-1 ");
        let (first, got) = home_get(&client, &key).await.unwrap();
        assert_eq!(
            (text(&got), got.is_some()),
            ("".into(), steps[0]["result"]["found"].as_bool().unwrap())
        );
        assert_eq!(calls(), steps[0]["calls"], "get_reserves");
        assert_eq!(home_replace(&client, &key, &first, content).await, Ok(true));
        assert_eq!(calls(), steps[1]["calls"], "replace");
        let (second, got) = home_get(&client, &key).await.unwrap();
        assert_eq!(text(&got), steps[2]["result"]["content"].as_str().unwrap());
        assert_eq!(calls(), steps[2]["calls"], "get_content");
        assert_eq!(home_replace(&client, &key, &first, content).await, Ok(false));
        assert_eq!(calls(), steps[3]["calls"], "replace_stale");
        assert_eq!(home_delete(&client, &key, &second).await, Ok(true));
        assert_eq!(calls(), steps[4]["calls"], "delete");
        let (_, got) = home_get(&client, &key).await.unwrap();
        assert!(got.is_none());
        assert_eq!(calls(), steps[5]["calls"], "get_deleted");
        values.lock().unwrap().insert(
            kv_key("k3", "legacy"),
            r#"[{"type":"thinking","thinking":"x","signature":"s"}]"#.into(),
        );
        let (_, got) = home_get(&client, &kv_key("k3", "legacy")).await.unwrap();
        assert_eq!(text(&got), steps[6]["result"]["content"].as_str().unwrap());
        assert_eq!(calls(), steps[6]["calls"], "get_legacy");
        values
            .lock()
            .unwrap()
            .insert(kv_key("k3", "bad"), r#"{"generation":" "}"#.into());
        let error = home_get(&client, &kv_key("k3", "bad")).await.unwrap_err();
        assert_eq!(error, steps[7]["result"]["error_text"].as_str().unwrap());
        assert_eq!(calls(), steps[7]["calls"], "get_invalid");
    }
}
