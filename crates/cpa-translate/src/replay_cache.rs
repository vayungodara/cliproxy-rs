//! Adapter for internal/cache's Antigravity reasoning replay cache
//! (antigravity_reasoning_replay_cache.go), which the Gemini Responses translator uses to
//! keep thought signatures that trail visible text out of the client-visible output.
//!
//! ponytail: owner is whoever ports internal/cache with the Antigravity executor (Google
//! thread). This is the in-process store only, keyed, normalized and bounded like Go (1 h
//! sliding TTL, 10,240 entries, oldest 128 evicted); Go's Home KV backend, snapshots,
//! compare-and-swap writes and absent-key tombstones are not ported, and only
//! `thought_signature` items (the kind translators write) are accepted. Swap both
//! functions for the shared cache at integration.

use std::collections::HashMap;
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
