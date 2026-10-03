//! Adapters for two internal/cache stores: the Antigravity reasoning replay cache
//! (antigravity_reasoning_replay_cache.go), which the Gemini Responses translator uses to
//! keep thought signatures that trail visible text out of the client-visible output, and
//! the thinking signature cache (signature_cache.go), which the Claude -> Antigravity
//! translators use to recover signatures of thinking text the client sent back unsigned.
//!
//! ponytail: owner is whoever ports internal/cache with the Antigravity executor (Google
//! thread). The replay cache is the in-process store only, keyed, normalized and bounded
//! like Go (1 h sliding TTL, 10,240 entries, oldest 128 evicted); Go's Home KV backend,
//! snapshots, compare-and-swap writes and absent-key tombstones are not ported, and only
//! `thought_signature` items (the kind translators write) are accepted. Swap these
//! functions for the shared cache at integration.

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

struct Entry {
    items: Vec<Vec<u8>>,
    at: Instant,
}

static ENTRIES: LazyLock<Mutex<HashMap<String, Entry>>> = LazyLock::new(Default::default);

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
    let mut entries = ENTRIES.lock().unwrap_or_else(PoisonError::into_inner);
    entries.insert(
        key,
        Entry {
            items,
            at: Instant::now(),
        },
    );
    if entries.len() > MAX_ENTRIES {
        let mut oldest: Vec<(Instant, String)> = entries.iter().map(|(k, e)| (e.at, k.clone())).collect();
        oldest.sort();
        for (_, k) in oldest.into_iter().take(EVICT_BATCH) {
            entries.remove(&k);
        }
    }
    true
}

/// cache.GetAntigravityReasoningReplayItems: a hit refreshes the entry's TTL.
pub(crate) fn get(model: &str, session: &str) -> Option<Vec<Vec<u8>>> {
    let key = key(model, session)?;
    let mut entries = ENTRIES.lock().unwrap_or_else(PoisonError::into_inner);
    let now = Instant::now();
    let entry = entries.get_mut(&key)?;
    if now.duration_since(entry.at) > TTL {
        entries.remove(&key);
        return None;
    }
    entry.at = now;
    Some(entry.items.clone())
}

// ---------------------------------------------------------------------------------------
// Thinking signature cache (internal/cache/signature_cache.go)
//
// ponytail: same owner and swap as above. In-process store only: Go's Home KV backend is
// not ported, and the 10-minute background purge is replaced by expiry on read.

const SIGNATURE_TTL: Duration = Duration::from_secs(3 * 3600);
const MIN_VALID_SIGNATURE_LEN: usize = 50;
const GEMINI_BYPASS: &str = "skip_thought_signature_validator";

type SignatureGroups = HashMap<String, HashMap<String, (String, Instant)>>;

static SIGNATURES: LazyLock<Mutex<SignatureGroups>> = LazyLock::new(Default::default);
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
    let mut groups = SIGNATURES.lock().unwrap_or_else(PoisonError::into_inner);
    groups.entry(model_group(model).to_owned()).or_default().insert(
        text_hash(text),
        (String::from_utf8_lossy(signature).into_owned(), Instant::now()),
    );
    true
}

/// cache.GetCachedSignatureRequired: a hit refreshes the entry; a Gemini-group miss is
/// the bypass sentinel.
pub(crate) fn cached_signature(model: &str, text: &[u8]) -> String {
    let group = model_group(model);
    let miss = || {
        if group == "gemini" {
            GEMINI_BYPASS.to_owned()
        } else {
            String::new()
        }
    };
    if text.is_empty() {
        return miss();
    }
    let mut groups = SIGNATURES.lock().unwrap_or_else(PoisonError::into_inner);
    let Some(entries) = groups.get_mut(group) else {
        return miss();
    };
    let hash = text_hash(text);
    let now = Instant::now();
    match entries.get_mut(&hash) {
        Some((_, at)) if now.duration_since(*at) > SIGNATURE_TTL => {
            entries.remove(&hash);
            miss()
        }
        Some((signature, at)) => {
            *at = now;
            signature.clone()
        }
        None => miss(),
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
}
