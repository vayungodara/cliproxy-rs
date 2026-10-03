//! Claude-compatible thinking replay (claude_thinking_replay.go,
//! internal/cache/claude_thinking_replay_cache.go).
//!
//! For is-compat Claude API-key models, complete assistant content carrying signed
//! thinking and a tool call is cached per (credential and model, session). Unlike Kimi's
//! single entry, a session keeps up to 64 distinct turns; each request restores every
//! cached turn whose non-thinking parts match an unsigned assistant turn. Writes are
//! conditional on the generation the request read, so a slower request never
//! overwrites newer content.
//!
//! ponytail: in-process cache only; Go's Home KV backend is not ported.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use bytes::Bytes;
use cpa_common::json as gj;
use cpa_core::credential::Credential;
use cpa_core::exec::{ExecRequest, ExecStream};
use sha2::{Digest, Sha256};

const TTL: Duration = Duration::from_secs(3600);
const MAX_ENTRIES: usize = 10240;
const EVICT_BATCH: usize = 128;
const MAX_BYTES_PER_SESSION: usize = MAX_BYTES_PER_ENTRY;
const MAX_TURNS_PER_SESSION: usize = 64;
const MAX_TOTAL_BYTES: usize = 256 << 20;
const SWEEP_INTERVAL: Duration = Duration::from_secs(600);

use crate::replay::{Accumulator, MAX_BLOCKS_PER_ENTRY, MAX_BYTES_PER_ENTRY, ReplayTarget};

/// `claudeThinkingReplayJSONEqual`: both decode, and their Go encodings are equal.
fn json_equal(left: &[u8], right: &[u8]) -> bool {
    match (cpa_common::json::canonical(left), cpa_common::json::canonical(right)) {
        (Some(a), Some(b)) => a == b,
        _ => false,
    }
}

#[derive(Clone)]
struct Entry {
    contents: Vec<Arc<[u8]>>,
    at: Instant,
    generation: u64,
    deleted: bool,
}

impl Entry {
    fn bytes(&self) -> usize {
        self.contents.iter().map(|c| c.len()).sum()
    }
}

#[derive(Default)]
struct Inner {
    entries: HashMap<String, Entry>,
    total: usize,
    next_generation: u64,
    /// When the last expiry sweep ran (Go's ten-minute cache cleanup ticker).
    swept: Option<Instant>,
}

impl Inner {
    fn generation(&mut self) -> u64 {
        self.next_generation += 1;
        self.next_generation
    }

    fn put(&mut self, key: &str, entry: Entry) {
        if let Some(old) = self.entries.insert(key.to_owned(), entry) {
            self.total -= old.bytes();
        }
        self.total += self.entries[key].bytes();
    }

    fn remove(&mut self, key: &str) {
        if let Some(old) = self.entries.remove(key) {
            self.total -= old.bytes();
        }
    }

    /// `purgeExpiredClaudeThinkingReplayCache`, run when Go's ten-minute cleanup would
    /// have: expired entries are removed, so their old generations stop accepting writes.
    // ponytail: swept lazily on cache access instead of by a background ticker; an idle
    // cache keeps expired content in memory until the next request.
    fn sweep(&mut self, now: Instant) {
        if self.swept.is_some_and(|at| now.duration_since(at) < SWEEP_INTERVAL) {
            return;
        }
        self.swept = Some(now);
        let expired: Vec<String> = self
            .entries
            .iter()
            .filter(|(_, e)| now.duration_since(e.at) > TTL)
            .map(|(k, _)| k.clone())
            .collect();
        for key in expired {
            self.remove(&key);
        }
    }

    /// `enforceClaudeThinkingReplayLimitsLocked`: drop the oldest 128 at a time.
    fn enforce(&mut self) {
        while self.entries.len() > MAX_ENTRIES || self.total > MAX_TOTAL_BYTES {
            let mut oldest: Vec<(Instant, String)> = self.entries.iter().map(|(k, e)| (e.at, k.clone())).collect();
            oldest.sort();
            for (_, key) in oldest.into_iter().take(EVICT_BATCH) {
                self.remove(&key);
            }
        }
    }
}

/// The generation a request read (Go `ClaudeThinkingReplaySnapshot`).
#[derive(Clone, Copy)]
struct Snapshot(u64);

/// Process-wide Claude replay cache.
#[derive(Default)]
pub(crate) struct ReplayCache(Mutex<Inner>);

/// The process's cache, shared by every executor like Go's package-level cache.
pub(crate) fn shared() -> Arc<ReplayCache> {
    static SHARED: std::sync::OnceLock<Arc<ReplayCache>> = std::sync::OnceLock::new();
    SHARED.get_or_init(Arc::default).clone()
}

impl ReplayCache {
    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.0.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// `GetClaudeThinkingReplayWithSnapshotRequired`: a missing or expired entry is
    /// reserved as deleted so a conditional write can follow.
    fn get(&self, key: &str, now: Instant) -> (Vec<Arc<[u8]>>, Snapshot) {
        let mut inner = self.lock();
        inner.sweep(now);
        let live = inner
            .entries
            .get(key)
            .filter(|e| now.duration_since(e.at) <= TTL)
            .cloned();
        let mut entry = match live {
            Some(entry) => entry,
            None => {
                inner.remove(key);
                let generation = inner.generation();
                let reserved = Entry {
                    contents: Vec::new(),
                    at: now,
                    generation,
                    deleted: true,
                };
                inner.put(key, reserved.clone());
                inner.enforce();
                reserved
            }
        };
        entry.at = now;
        if let Some(stored) = inner.entries.get_mut(key) {
            stored.at = now;
        }
        let contents = if entry.deleted { Vec::new() } else { entry.contents };
        (contents, Snapshot(entry.generation))
    }

    /// `ReplaceClaudeThinkingReplayIfUnchanged` with `appendClaudeThinkingReplayContent`.
    fn append_if_unchanged(&self, key: &str, snapshot: Snapshot, content: &[u8]) -> bool {
        self.append_at(key, snapshot, content, Instant::now())
    }

    fn append_at(&self, key: &str, snapshot: Snapshot, content: &[u8], now: Instant) -> bool {
        if !valid_content(content) {
            return false;
        }
        let mut inner = self.lock();
        inner.sweep(now);
        let Some(entry) = inner.entries.get(key).filter(|e| e.generation == snapshot.0) else {
            return false;
        };
        let mut contents = if entry.deleted {
            Vec::new()
        } else {
            entry.contents.clone()
        };
        if !contents.iter().any(|existing| json_equal(existing, content)) {
            contents.push(Arc::from(content));
            while contents.len() > MAX_TURNS_PER_SESSION
                || contents.iter().map(|c| c.len()).sum::<usize>() > MAX_BYTES_PER_SESSION
            {
                contents.remove(0);
            }
        }
        let generation = inner.generation();
        inner.put(
            key,
            Entry {
                contents,
                at: now,
                generation,
                deleted: false,
            },
        );
        inner.enforce();
        true
    }

    /// `DeleteClaudeThinkingReplayIfUnchanged`: a tombstone with a new generation.
    fn delete_if_unchanged(&self, key: &str, snapshot: Snapshot) -> bool {
        let mut inner = self.lock();
        if inner.entries.get(key).is_none_or(|e| e.generation != snapshot.0) {
            return false;
        }
        let generation = inner.generation();
        inner.put(
            key,
            Entry {
                contents: Vec::new(),
                at: Instant::now(),
                generation,
                deleted: true,
            },
        );
        true
    }
}

/// `validClaudeThinkingReplayContent`.
fn valid_content(content: &[u8]) -> bool {
    if content.is_empty() || content.len() > MAX_BYTES_PER_SESSION || !gj::valid(content) {
        return false;
    }
    let root = gj::parse(content);
    root.is_array() && (1..=MAX_BLOCKS_PER_ENTRY).contains(&root.array().len())
}

/// One request's replay scope (Go `claudeThinkingReplayScope`).
pub(crate) struct Scope {
    cache: Arc<ReplayCache>,
    key: String,
    snapshot: Snapshot,
    pub(crate) applied: bool,
}

impl Scope {
    /// `cacheClaudeThinkingReplayContent`: append replayable content, else clear.
    fn store(&self, content: &[u8]) {
        if crate::replay::replayable(content) {
            self.cache.append_if_unchanged(&self.key, self.snapshot, content);
        } else {
            self.clear();
        }
    }

    /// `clearClaudeThinkingReplayContent`.
    pub(crate) fn clear(&self) {
        self.cache.delete_if_unchanged(&self.key, self.snapshot);
    }

    /// `cacheClaudeThinkingReplayResponse`: the `content` array of a buffered message,
    /// else the content a buffered SSE body completes.
    pub(crate) fn store_response(&self, response: &[u8]) {
        let content = gj::get(response, "content");
        if content.is_array() {
            self.store(content.raw());
            return;
        }
        let mut accumulator = Accumulator::default();
        accumulator.observe(response);
        if let Some(content) = accumulator.content() {
            self.store(&content);
        }
    }

    /// `wrapClaudeThinkingReplayStream`.
    pub(crate) fn wrap(self, stream: ExecStream) -> ExecStream {
        crate::replay::wrap_stream(stream, self)
    }
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

/// `claudeThinkingReplayModelFamily`: the base model, scoped to the credential.
fn model_family(credential: &Credential, api_key: &str, base_url: &str, base_model: &str) -> String {
    let base = base_model.trim();
    if base.is_empty() {
        return String::new();
    }
    let identity = [credential.id.trim(), base_url.trim(), api_key.trim()]
        .into_iter()
        .find(|s| !s.is_empty())
        .unwrap_or_default();
    if identity.is_empty() {
        return format!("claude:{base}");
    }
    let digest = Sha256::digest(identity.as_bytes());
    let prefix: String = digest[..8].iter().map(|b| format!("{b:02x}")).collect();
    format!("claude:{prefix}:{base}")
}

/// What `claudeThinkingReplayEnabled` reads from the attempt.
pub(crate) struct Gate<'a> {
    pub credential: &'a Credential,
    pub api_key: &'a str,
    /// The credential's own `base_url` attribute (`claudeCreds`).
    pub base_url: &'a str,
    pub base_model: &'a str,
    pub is_compat: bool,
    pub oauth_token: bool,
}

/// `prepareClaudeThinkingReplayRequest` when `claudeThinkingReplayEnabled`: Claude
/// clients of an is-compat Claude API-key model. Restores every cached turn into
/// `req.body`. `None` when replay does not apply or no session identity exists.
pub(crate) fn prepare(cache: &Arc<ReplayCache>, gate: &Gate<'_>, req: &mut ExecRequest) -> Option<Scope> {
    let enabled = req.source_format == cpa_core::format::Format::Claude
        && gate.credential.provider.trim().eq_ignore_ascii_case("claude")
        && auth_kind_is_api_key(gate.credential)
        && gate.is_compat
        && !gate.api_key.trim().is_empty()
        && !gate.oauth_token;
    if !enabled {
        return None;
    }
    let family = model_family(gate.credential, gate.api_key, gate.base_url, gate.base_model);
    let session = crate::replay::session_key(req, &req.body);
    if family.is_empty() || session.trim().is_empty() {
        return None;
    }
    let key = format!("claude-thinking-replay\0{family}\0{}", session.trim());
    let (contents, snapshot) = cache.get(&key, Instant::now());
    let mut body: Option<Vec<u8>> = None;
    for cached in &contents {
        if let Some(updated) = crate::replay::restore_turn(body.as_deref().unwrap_or(&req.body), cached) {
            body = Some(updated);
        }
    }
    let applied = body.is_some();
    if let Some(body) = body {
        req.body = Bytes::from(body);
    }
    Some(Scope {
        cache: cache.clone(),
        key,
        snapshot,
        applied,
    })
}

/// `Auth.AuthKind() == AuthKindAPIKey`: a recognised attribute kind, then a recognised
/// metadata kind, then a non-empty `api_key` attribute.
fn auth_kind_is_api_key(credential: &Credential) -> bool {
    let normalize = |kind: &str| match kind.trim().to_lowercase().as_str() {
        "apikey" | "api_key" | "api-key" => Some(true),
        "oauth" | "oauth2" => Some(false),
        _ => None,
    };
    let attribute = credential
        .attributes
        .get("auth_kind")
        .map(String::as_str)
        .unwrap_or_default();
    normalize(attribute)
        .or_else(|| normalize(credential.str("auth_kind").unwrap_or_default()))
        .unwrap_or_else(|| {
            credential
                .attributes
                .get("api_key")
                .is_some_and(|k| !k.trim().is_empty())
        })
}

/// `shouldClearKimiThinkingReplayAfterError`: only an upstream 400 or 422 classified as
/// Go's plain `statusErr` rejects applied replay. Callers ask this of upstream responses
/// only: local validation errors are other Go types, and a Fast request's direct answer
/// (`claudeFastDirectResponseError`) unwraps to `RequestTerminatedError`, so both keep
/// the cache.
pub(crate) fn upstream_rejects(error: &cpa_core::exec::ExecError) -> bool {
    !error.direct && matches!(error.status, 400 | 422)
}

#[cfg(test)]
mod tests {
    use super::*;

    const TURN_A: &str = r#"[{"type":"thinking","thinking":"a","signature":"sig-a"},{"type":"tool_use","id":"toolu_a","name":"read","input":{"path":"a"}}]"#;
    const TURN_B: &str = r#"[{"type":"thinking","thinking":"b","signature":"sig-b"},{"type":"tool_use","id":"toolu_b","name":"read","input":{"path":"b"}}]"#;

    fn scope(cache: &Arc<ReplayCache>) -> Scope {
        let (_, snapshot) = cache.get("k", Instant::now());
        Scope {
            cache: cache.clone(),
            key: "k".into(),
            snapshot,
            applied: false,
        }
    }

    #[test]
    fn sessions_keep_distinct_turns_and_writes_need_the_read_generation() {
        let cache = Arc::new(ReplayCache::default());
        let first = scope(&cache);
        first.store(TURN_A.as_bytes());
        // A stale scope (read before the write) cannot append or clear.
        first.store(TURN_B.as_bytes());
        first.clear();
        assert_eq!(cache.get("k", Instant::now()).0.len(), 1);
        scope(&cache).store(TURN_B.as_bytes());
        // An equal turn (key order differs) is not appended twice.
        scope(&cache).store(
            br#"[{"signature":"sig-a","type":"thinking","thinking":"a"},{"type":"tool_use","name":"read","id":"toolu_a","input":{"path":"a"}}]"#,
        );
        let contents = cache.get("k", Instant::now()).0;
        assert_eq!(
            contents.iter().map(|c| &**c).collect::<Vec<_>>(),
            [TURN_A.as_bytes(), TURN_B.as_bytes()]
        );
        // Content without signed thinking plus a tool call clears the session.
        scope(&cache).store(br#"[{"type":"text","text":"done"}]"#);
        assert!(cache.get("k", Instant::now()).0.is_empty());
    }

    #[test]
    fn expired_entries_are_swept_and_their_generations_stop_accepting_writes() {
        let cache = Arc::new(ReplayCache::default());
        let start = Instant::now();
        let (_, snapshot) = cache.get("k", start);
        assert!(cache.append_at("k", snapshot, TURN_A.as_bytes(), start));
        let (_, pending) = cache.get("k", start);
        // Past the TTL and a sweep interval, a write from the old read is rejected.
        let later = start + TTL + SWEEP_INTERVAL + Duration::from_secs(1);
        assert!(!cache.append_at("k", pending, TURN_B.as_bytes(), later));
        assert!(cache.lock().entries.is_empty());
        assert_eq!(cache.lock().total, 0);
    }

    #[test]
    fn the_gate_classifies_credentials_like_go_auth_kind() {
        let credential = |attrs: &[(&str, &str)], meta: serde_json::Value| {
            let mut c = Credential::from_file(
                std::path::Path::new("/a"),
                std::path::Path::new("/a/c.json"),
                meta.as_object().unwrap().clone(),
            )
            .unwrap();
            c.attributes = attrs.iter().map(|(k, v)| ((*k).into(), (*v).into())).collect();
            c
        };

        assert!(auth_kind_is_api_key(&credential(
            &[("api_key", "k")],
            serde_json::json!({"type":"claude"})
        )));
        assert!(!auth_kind_is_api_key(&credential(
            &[("api_key", "k")],
            serde_json::json!({"type":"claude","auth_kind":"oauth"})
        )));
        assert!(auth_kind_is_api_key(&credential(
            &[("auth_kind", "weird"), ("api_key", "k")],
            serde_json::json!({"type":"claude"})
        )));
        assert!(!auth_kind_is_api_key(&credential(
            &[("auth_kind", "OAuth2"), ("api_key", "k")],
            serde_json::json!({"type":"claude"})
        )));
        assert!(!auth_kind_is_api_key(&credential(
            &[],
            serde_json::json!({"type":"claude","api_key":"meta-only"})
        )));
    }

    #[test]
    fn only_classified_upstream_rejections_clear() {
        use cpa_core::exec::{ExecError, FailureScope};
        let classified = |status| ExecError::local(status, FailureScope::Request, "rejected");
        assert!(upstream_rejects(&classified(400)));
        assert!(upstream_rejects(&classified(422)));
        assert!(!upstream_rejects(&classified(429)));
        assert!(!upstream_rejects(&classified(500)));
        // A Fast request's direct 400 is answered as sent and keeps the cache.
        let mut direct = classified(400);
        direct.direct = true;
        assert!(!upstream_rejects(&direct));
    }

    #[test]
    fn a_session_keeps_at_most_64_turns() {
        let cache = Arc::new(ReplayCache::default());
        for i in 0..70 {
            scope(&cache).store(TURN_A.replace("toolu_a", &format!("toolu_{i}")).as_bytes());
        }
        let contents = cache.get("k", Instant::now()).0;
        assert_eq!(contents.len(), MAX_TURNS_PER_SESSION);
        assert!(
            String::from_utf8_lossy(&contents[0]).contains("toolu_6\""),
            "the oldest turns are dropped first"
        );
    }

    #[test]
    fn model_family_hashes_the_credential_identity() {
        let credential = Credential::from_file(
            std::path::Path::new("/a"),
            std::path::Path::new("/a/c.json"),
            serde_json::json!({"type":"claude"}).as_object().unwrap().clone(),
        )
        .unwrap();
        let digest = Sha256::digest(credential.id.as_bytes());
        let prefix: String = digest[..8].iter().map(|b| format!("{b:02x}")).collect();
        assert_eq!(
            model_family(&credential, "k", "", "claude-x"),
            format!("claude:{prefix}:claude-x")
        );
        assert_eq!(model_family(&credential, "k", "", " "), "");
    }
}
