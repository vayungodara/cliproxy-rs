//! Adapters for two internal/cache stores: the Antigravity reasoning replay cache
//! (antigravity_reasoning_replay_cache.go), which the Gemini Responses translator uses to
//! keep thought signatures that trail visible text out of the client-visible output, and
//! the thinking signature cache (signature_cache.go), which the Claude -> Antigravity
//! translators use to recover signatures of thinking text the client sent back unsigned.
//!
//! ponytail: owner is whoever ports internal/cache with the Antigravity executor (Google
//! thread). The replay cache is the in-process store only, keyed, normalized and bounded
//! like Go (1 h sliding TTL, 10,240 entries including absent-key tombstones, oldest 128
//! evicted); Go's Home KV backend, snapshots, revisions and compare-and-swap writes (used
//! by executors, not translators) are not ported, and only `thought_signature` items (the
//! kind translators write) are accepted. Go's 10-minute background purge runs lazily, at
//! the nominal tick times, on the next cache access (`Caches::sweep`). Go purges when its
//! goroutine wakes, a scheduler-dependent instant after the tick, so an entry exactly at
//! its TTL on a tick can differ within that jitter; Go's own timing is not deterministic
//! there either. Swap these functions for the shared cache at integration.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{LazyLock, Mutex, PoisonError};
use std::time::{Duration, Instant};

use cpa_common::json::{self as gj, Kind};

use crate::common::trim_space;

const TTL: Duration = Duration::from_secs(3600);
const MAX_ENTRIES: usize = 10240;
const EVICT_BATCH: usize = 128;
const MIN_SIGNATURE_LEN: usize = 16;
const MAX_ITEMS_PER_ENTRY: usize = 4096;
const MAX_BYTES_PER_ENTRY: usize = 16 << 20;

/// One replay entry; `items` is empty for an absent-key tombstone (Go's `Deleted`).
struct Entry {
    items: Vec<Vec<u8>>,
    at: Instant,
}

/// CacheCleanupInterval: Go's purge ticker, started by the first write to the signature
/// cache or the first access to the replay cache (`cacheCleanupOnce`).
const CLEANUP_INTERVAL: Duration = Duration::from_secs(600);

/// Both in-process stores and Go's shared purge ticker, behind one lock.
#[derive(Default)]
struct Caches {
    replay: HashMap<String, Entry>,
    /// model group -> text hash -> (signature, last use).
    signatures: HashMap<String, HashMap<String, (Vec<u8>, Instant)>>,
    /// (ticker start, ticks already purged).
    ticker: Option<(Instant, u32)>,
}

static CACHES: LazyLock<Mutex<Caches>> = LazyLock::new(Default::default);

fn caches() -> std::sync::MutexGuard<'static, Caches> {
    CACHES.lock().unwrap_or_else(PoisonError::into_inner)
}

impl Caches {
    /// purgeExpiredCaches for every ticker tick that passed since the last access. No
    /// access happens in between, so purging once at the latest missed tick time removes
    /// exactly what Go's ticks would have removed by now. `start`: whether this access
    /// starts Go's ticker (signature reads do not).
    fn sweep(&mut self, now: Instant, start: bool) {
        if self.ticker.is_none() && !start {
            return;
        }
        let (begin, done) = self.ticker.get_or_insert((now, 0));
        let ticks = (now.duration_since(*begin).as_secs() / CLEANUP_INTERVAL.as_secs()) as u32;
        if ticks <= *done {
            return;
        }
        *done = ticks;
        let tick_at = *begin + CLEANUP_INTERVAL * ticks;
        self.signatures.retain(|_, entries| {
            entries.retain(|_, (_, at)| tick_at.saturating_duration_since(*at) <= SIGNATURE_TTL);
            !entries.is_empty()
        });
        self.replay
            .retain(|_, e| tick_at.saturating_duration_since(e.at) <= TTL);
    }

    /// evictOldestAntigravityReasoningReplayEntries.
    fn evict_oldest(&mut self) {
        let mut oldest: Vec<(Instant, String)> = self.replay.iter().map(|(k, e)| (e.at, k.clone())).collect();
        oldest.sort();
        for (_, k) in oldest.into_iter().take(EVICT_BATCH) {
            self.replay.remove(&k);
        }
    }

    fn put(&mut self, key: String, items: Vec<Vec<u8>>, now: Instant) {
        self.sweep(now, true);
        self.replay.insert(key, Entry { items, at: now });
        if self.replay.len() > MAX_ENTRIES {
            self.evict_oldest();
        }
    }

    /// A hit refreshes the entry (tombstones included); a miss or an expired entry
    /// reserves a tombstone that counts toward capacity
    /// (reserveAntigravityReasoningReplayAbsentLocked).
    fn get(&mut self, key: String, now: Instant) -> Option<Vec<Vec<u8>>> {
        self.sweep(now, true);
        if let Some(entry) = self.replay.get_mut(&key) {
            if now.duration_since(entry.at) <= TTL {
                entry.at = now;
                return (!entry.items.is_empty()).then(|| entry.items.clone());
            }
            self.replay.remove(&key);
        }
        if self.replay.len() >= MAX_ENTRIES {
            self.evict_oldest();
        }
        self.replay.insert(key, Entry { items: vec![], at: now });
        None
    }

    fn cache_signature(&mut self, group: &str, hash: String, signature: &[u8], now: Instant) {
        self.sweep(now, true);
        self.signatures
            .entry(group.to_owned())
            .or_default()
            .insert(hash, (signature.to_vec(), now));
    }

    /// A hit refreshes the entry; an expired entry is removed.
    fn cached_signature(&mut self, group: &str, hash: &str, now: Instant) -> Option<Vec<u8>> {
        self.sweep(now, false);
        let entries = self.signatures.get_mut(group)?;
        let (signature, at) = entries.get_mut(hash)?;
        if now.duration_since(*at) > SIGNATURE_TTL {
            entries.remove(hash);
            return None;
        }
        *at = now;
        Some(signature.clone())
    }
}

fn key(model: &str, session: &str) -> Option<String> {
    let (model, session) = (model.trim(), session.trim());
    (!model.is_empty() && !session.is_empty()).then(|| ["antigravity-reasoning-replay", model, session].join("\x00"))
}

/// normalizeAntigravityThoughtSignatureReplayItem.
fn normalize_item(item: &[u8]) -> Option<Vec<u8>> {
    let r = gj::parse(item);
    if trim_space(&r.get("type").bytes()) != b"thought_signature" {
        return None;
    }
    let mut sig = trim_space(&r.get("thoughtSignature").bytes()).to_vec();
    if sig.is_empty() {
        sig = trim_space(&r.get("thought_signature").bytes()).to_vec();
    }
    if sig.is_empty() || sig == b"skip_thought_signature_validator" || sig.len() < MIN_SIGNATURE_LEN {
        return None;
    }
    let mut out = br#"{"type":"thought_signature"}"#.to_vec();
    gj::set_str(&mut out, "thoughtSignature", &sig);
    for field in ["contentIndex", "partIndex"] {
        let v = r.get(field);
        if v.kind == Kind::Number {
            gj::set_int(&mut out, field, v.int());
        }
    }
    let target_kind = trim_space(&r.get("targetKind").bytes()).to_vec();
    if target_kind == b"text" || target_kind == b"thought" {
        gj::set_str(&mut out, "targetKind", &target_kind);
    }
    let target_hash = trim_space(&r.get("targetHash").bytes()).to_vec();
    if !target_hash.is_empty() {
        gj::set_str(&mut out, "targetHash", &target_hash);
    }
    let occurrence = r.get("targetOccurrence");
    if occurrence.kind == Kind::Number && occurrence.int() >= 0 {
        gj::set_int(&mut out, "targetOccurrence", occurrence.int());
    }
    let context_hash = trim_space(&r.get("contextHash").bytes()).to_vec();
    if !context_hash.is_empty() {
        gj::set_str(&mut out, "contextHash", &context_hash);
    }
    Some(out)
}

/// normalizeAntigravityReasoningReplayItems.
fn normalize(items: &[Vec<u8>]) -> Option<Vec<Vec<u8>>> {
    if items.len() > MAX_ITEMS_PER_ENTRY {
        return None;
    }
    let mut out = vec![];
    let mut total = 0;
    for item in items.iter().filter_map(|i| normalize_item(i)) {
        total += item.len();
        if total > MAX_BYTES_PER_ENTRY {
            return None;
        }
        out.push(item);
    }
    (!out.is_empty()).then_some(out)
}

/// cache.CacheAntigravityReasoningReplayItems.
pub(crate) fn put(model: &str, session: &str, items: &[Vec<u8>]) -> bool {
    let (Some(key), Some(items)) = (key(model, session), normalize(items)) else {
        return false;
    };
    caches().put(key, items, Instant::now());
    true
}

/// cache.GetAntigravityReasoningReplayItems.
pub(crate) fn get(model: &str, session: &str) -> Option<Vec<Vec<u8>>> {
    let key = key(model, session)?;
    caches().get(key, Instant::now())
}

// ---------------------------------------------------------------------------------------
// Thinking signature cache (internal/cache/signature_cache.go)
//
// ponytail: same owner and swap as above. In-process store only: Go's Home KV backend is
// not ported. Expired entries and empty groups go in `Caches::sweep`, as in Go.

const SIGNATURE_TTL: Duration = Duration::from_secs(3 * 3600);
const MIN_VALID_SIGNATURE_LEN: usize = 50;
const GEMINI_BYPASS: &str = "skip_thought_signature_validator";

static SIGNATURE_CACHE_ENABLED: AtomicBool = AtomicBool::new(true);
static SIGNATURE_BYPASS_STRICT: AtomicBool = AtomicBool::new(false);

/// cache.SetSignatureCacheEnabled and SetSignatureBypassStrictMode (Go applies config
/// `antigravity-signature-cache-enabled`, default true, and
/// `antigravity-signature-bypass-strict`, default false, on load and reload).
pub fn set_signature_cache_config(enabled: bool, bypass_strict: bool) {
    SIGNATURE_CACHE_ENABLED.store(enabled, Ordering::Relaxed);
    SIGNATURE_BYPASS_STRICT.store(bypass_strict, Ordering::Relaxed);
}

/// cache.SignatureCacheEnabled.
pub(crate) fn signature_cache_enabled() -> bool {
    SIGNATURE_CACHE_ENABLED.load(Ordering::Relaxed)
}

/// cache.SignatureBypassStrictMode.
pub(crate) fn signature_bypass_strict() -> bool {
    SIGNATURE_BYPASS_STRICT.load(Ordering::Relaxed)
}

/// cache.GetModelGroup.
pub(crate) fn model_group(model: &str) -> &str {
    if model.contains("gpt") {
        "gpt"
    } else if model.contains("claude") {
        "claude"
    } else if model.contains("gemini") {
        "gemini"
    } else {
        model
    }
}

fn text_hash(text: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    crate::common::hex(&Sha256::digest(text))[..16].to_owned()
}

/// cache.CacheSignatureBestEffort.
pub(crate) fn cache_signature(model: &str, text: &[u8], signature: &[u8]) -> bool {
    if text.is_empty() || signature.is_empty() || signature.len() < MIN_VALID_SIGNATURE_LEN {
        return false;
    }
    caches().cache_signature(model_group(model), text_hash(text), signature, Instant::now());
    true
}

/// cache.GetCachedSignatureRequired: a Gemini-group miss is the bypass sentinel.
pub(crate) fn cached_signature(model: &str, text: &[u8]) -> Vec<u8> {
    let group = model_group(model);
    let hit = if text.is_empty() {
        None
    } else {
        caches().cached_signature(group, &text_hash(text), Instant::now())
    };
    match hit {
        Some(signature) => signature,
        None if group == "gemini" => GEMINI_BYPASS.as_bytes().to_vec(),
        None => vec![],
    }
}

/// cache.HasValidSignature.
pub(crate) fn has_valid_signature(model: &str, signature: &[u8]) -> bool {
    (!signature.is_empty() && signature.len() >= MIN_VALID_SIGNATURE_LEN)
        || (signature == GEMINI_BYPASS.as_bytes() && model_group(model) == "gemini")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stores_normalized_thought_signatures_only() {
        let items = vec![
            br#"{"type":"thought_signature","thoughtSignature":" sig-aaaaaaaaaaaaaaaa ","targetKind":"text","targetHash":"h","extra":1}"#.to_vec(),
            br#"{"type":"thought_signature","thoughtSignature":"short"}"#.to_vec(),
            br#"{"type":"other","thoughtSignature":"sig-bbbbbbbbbbbbbbbb"}"#.to_vec(),
        ];
        assert!(put("replay-test-model", "replay-test-session", &items));
        assert_eq!(
            get("replay-test-model", "replay-test-session").unwrap(),
            vec![br#"{"type":"thought_signature","thoughtSignature":"sig-aaaaaaaaaaaaaaaa","targetKind":"text","targetHash":"h"}"#.to_vec()]
        );
        assert!(!put("replay-test-model", "replay-test-empty", &items[1..]));
        assert!(get("replay-test-model", "replay-test-empty").is_none());
        assert!(!put(" ", "s", &items));
    }

    fn min(n: u64) -> Duration {
        Duration::from_secs(60 * n)
    }

    #[test]
    fn misses_reserve_tombstones_that_count_toward_capacity() {
        let (mut c, t0) = (Caches::default(), Instant::now());
        c.put("a".into(), vec![b"item".to_vec()], t0);
        // 10,239 distinct misses fill the map; the next one evicts the oldest 128, `a` first.
        for i in 1..MAX_ENTRIES {
            assert!(
                c.get(format!("absent-{i}"), t0 + Duration::from_micros(i as u64))
                    .is_none()
            );
        }
        assert_eq!(c.replay.len(), MAX_ENTRIES);
        assert_eq!(c.get("a".into(), t0 + min(1)).as_deref(), Some(&[b"item".to_vec()][..]));
        c.get("absent-last".into(), t0 + min(2));
        assert_eq!(c.replay.len(), MAX_ENTRIES - EVICT_BATCH + 1);
        assert!(c.replay.contains_key("a"), "refreshed by the hit");
        assert!(!c.replay.contains_key("absent-1"));
        // A tombstone hit stays a miss and refreshes the tombstone.
        assert!(c.get("absent-last".into(), t0 + min(3)).is_none());
        assert_eq!(c.replay["absent-last"].at, t0 + min(3));
    }

    #[test]
    fn expired_entries_purge_at_go_tick_times() {
        let (mut c, t0) = (Caches::default(), Instant::now());
        c.put("first".into(), vec![b"i".to_vec()], t0); // starts the ticker
        c.put("x".into(), vec![b"i".to_vec()], t0 + min(5));
        // Tick 6 (t0+60m): `first` is exactly 60m old (Go deletes only past the TTL) and
        // `x` 55m old, so both stay, although `x` is 61m old now.
        c.get("y".into(), t0 + min(66));
        assert!(c.replay.contains_key("x"));
        assert!(c.replay.contains_key("first"));
        c.get("z".into(), t0 + min(70) + Duration::from_secs(1));
        assert!(!c.replay.contains_key("x") && !c.replay.contains_key("first"));
        assert!(c.replay.contains_key("y") && c.replay.contains_key("z"));
    }

    #[test]
    fn signature_groups_expire_and_reads_do_not_start_the_ticker() {
        let (mut c, t0) = (Caches::default(), Instant::now());
        assert!(c.cached_signature("claude", "h", t0).is_none());
        assert!(c.ticker.is_none());
        let sig = b"\xff raw bytes".to_vec();
        c.cache_signature("claude", "h".into(), &sig, t0 + min(5));
        c.cache_signature("gemini", "g".into(), &sig, t0 + min(5));
        assert_eq!(c.ticker, Some((t0 + min(5), 0)));
        assert_eq!(c.cached_signature("claude", "h", t0 + min(100)), Some(sig.clone()));
        // gemini/g is 180m old at tick 18 (t0+185m): kept; past the TTL at tick 19.
        c.cached_signature("claude", "none", t0 + min(186));
        assert!(c.signatures.contains_key("gemini"));
        c.cached_signature("claude", "none", t0 + min(196));
        assert!(!c.signatures.contains_key("gemini"), "empty group removed");
        assert!(c.signatures.contains_key("claude"), "refreshed at t0+100m");
    }
}
