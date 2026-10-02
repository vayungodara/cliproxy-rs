//! Kimi native reasoning replay for Claude-format requests (kimi_thinking_replay.go,
//! internal/cache/kimi_thinking_replay_cache.go): Kimi's cache, model family and request
//! preparation on top of the shared replay helpers (crate::replay).
//!
//! After a complete response with signed thinking and a tool call, the assistant content
//! is cached per (model family, session); the next request restores it into the matching
//! assistant turn. Writes are conditional on the generation read, so a slower request
//! never overwrites newer content.
//!
//! ponytail: in-process cache only; Go's Home KV (cpa:kimi:*) backend is not ported.

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
        && let Some(updated) = restore_turn(&req.body, &content)
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
mod tests {
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
}
