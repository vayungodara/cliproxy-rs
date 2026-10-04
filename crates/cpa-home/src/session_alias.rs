//! Go sdk/cliproxy/auth/home_session_alias.go: one Home session ID per conversation.
//!
//! Clients name one conversation in several ways (a session header, a prompt cache key,
//! a conversation ID). Home's protocol carries a single session ID, so the node keeps
//! groups of aliases and sends the group's canonical ID; a group lives for the session
//! affinity TTL after its last use.

use std::collections::{BTreeMap, HashMap};
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant};

/// Go `defaultHomeSessionAliasTTL`.
pub const DEFAULT_TTL: Duration = Duration::from_secs(3600);
/// Go `homeSessionAliasCleanupOps`.
const CLEANUP_OPS: u64 = 256;
/// Go `homeSessionAliasSoftLimit`.
const SOFT_LIMIT: usize = 4096;
/// Go `maxStableSessionAliases`.
const MAX_STABLE_ALIASES: usize = 64;

#[derive(Debug, Clone, PartialEq)]
struct Group {
    canonical: String,
    expires: Instant,
    aliases: Vec<String>,
}

#[derive(Default)]
struct Inner {
    /// alias -> its group.
    entries: HashMap<String, Group>,
    /// canonical -> group.
    groups: HashMap<String, Group>,
    /// Insertion order of groups for the soft limit (Go's eviction list).
    order: BTreeMap<u64, String>,
    seq_of: HashMap<String, u64>,
    next_seq: u64,
    ops: u64,
    /// The TTL the groups were made with; a different one clears them (Go clears on a
    /// changed `routing.session-affinity-ttl`).
    ttl: Option<Duration>,
}

/// Go `homeSessionAliasCache`.
#[derive(Default)]
pub struct SessionAliases(Mutex<Inner>);

/// Go `mergeSessionAliases`: order kept, blanks and repeats dropped.
fn merge(existing: Vec<String>, candidates: &[String]) -> Vec<String> {
    let mut out: Vec<String> = Vec::with_capacity(existing.len() + candidates.len());
    for alias in existing.into_iter().chain(candidates.iter().cloned()) {
        if !alias.is_empty() && !out.contains(&alias) {
            out.push(alias);
        }
    }
    out
}

/// Go `compactHomeSessionAliases`: one prompt cache key (`pck:`) and at most 64 others.
fn compact(aliases: Vec<String>) -> Vec<String> {
    let mut prompt_key = false;
    let mut stable = 0;
    aliases
        .into_iter()
        .filter(|alias| {
            if alias.starts_with("pck:") {
                !std::mem::replace(&mut prompt_key, true)
            } else {
                stable += 1;
                stable <= MAX_STABLE_ALIASES
            }
        })
        .collect()
}

impl Inner {
    fn entry(&mut self, alias: &str, now: Instant) -> Option<Group> {
        let entry = self.entries.get(alias)?.clone();
        if now < entry.expires {
            return Some(entry);
        }
        match self.groups.get(&entry.canonical) {
            Some(group) if *group == entry => {
                let group = group.clone();
                self.remove_group(&group);
            }
            _ => {
                self.entries.remove(alias);
            }
        }
        None
    }

    fn group(&mut self, canonical: &str, now: Instant) -> Option<Group> {
        let group = self.groups.get(canonical)?.clone();
        if now < group.expires {
            return Some(group);
        }
        self.remove_group(&group);
        None
    }

    fn set_group(&mut self, group: Group) {
        if let Some(existing) = self.groups.get(&group.canonical).cloned() {
            self.remove_group(&existing);
        }
        for alias in &group.aliases {
            self.entries.insert(alias.clone(), group.clone());
        }
        self.next_seq += 1;
        self.order.insert(self.next_seq, group.canonical.clone());
        self.seq_of.insert(group.canonical.clone(), self.next_seq);
        self.groups.insert(group.canonical.clone(), group);
    }

    fn remove_group(&mut self, group: &Group) {
        if self.groups.get(&group.canonical) != Some(group) {
            return;
        }
        for alias in &group.aliases {
            if self.entries.get(alias) == Some(group) {
                self.entries.remove(alias);
            }
        }
        self.groups.remove(&group.canonical);
        if let Some(seq) = self.seq_of.remove(&group.canonical) {
            self.order.remove(&seq);
        }
    }

    fn enforce_limit(&mut self) {
        while self.entries.len() > SOFT_LIMIT {
            let Some((&seq, canonical)) = self.order.iter().next() else {
                return;
            };
            match self.groups.get(canonical).cloned() {
                Some(group) => self.remove_group(&group),
                None => {
                    let canonical = canonical.clone();
                    self.order.remove(&seq);
                    self.seq_of.remove(&canonical);
                }
            }
        }
    }

    fn cleanup(&mut self, now: Instant) {
        let expired: Vec<Group> = self.groups.values().filter(|g| now >= g.expires).cloned().collect();
        for group in expired {
            self.remove_group(&group);
        }
    }
}

impl SessionAliases {
    /// Go `homeSessionAliasCache.canonical`: the canonical ID of the group `primary`
    /// (and `fallback`, an alias of the same conversation) belongs to, merging groups
    /// they bridge. A zero `ttl` is the default hour.
    pub fn canonical(&self, primary: &str, fallback: &str, ttl: Duration, now: Instant) -> String {
        let (primary, fallback) = (primary.trim(), fallback.trim());
        if primary.is_empty() {
            return String::new();
        }
        let ttl = if ttl.is_zero() { DEFAULT_TTL } else { ttl };
        let mut inner = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        if inner.ttl != Some(ttl) {
            *inner = Inner {
                ttl: Some(ttl),
                ..Inner::default()
            };
        }
        inner.ops += 1;
        if inner.ops.is_multiple_of(CLEANUP_OPS) {
            inner.cleanup(now);
        }
        let mut canonical = primary.to_owned();
        let mut aliases = merge(Vec::new(), &[primary.to_owned(), fallback.to_owned()]);
        let mut previous: HashMap<String, Group> = HashMap::new();
        let mut primary_found = false;
        let mut from_live = false;
        if let Some(existing) = inner.entry(primary, now) {
            primary_found = true;
            from_live = true;
            canonical = existing.canonical.clone();
            aliases = merge(aliases, &existing.aliases);
            previous.insert(existing.canonical.clone(), existing);
        }
        if !fallback.is_empty()
            && fallback != primary
            && let Some(existing) = inner.entry(fallback, now)
        {
            from_live = true;
            if !primary_found {
                canonical = existing.canonical.clone();
            }
            aliases = merge(aliases, &existing.aliases);
            previous.insert(existing.canonical.clone(), existing);
        }
        if from_live {
            if let Some(existing) = inner.group(&canonical, now) {
                aliases = merge(aliases, &existing.aliases);
                previous.insert(existing.canonical.clone(), existing);
            }
        } else if inner.group(&canonical, now).is_some() {
            return canonical;
        }
        let aliases = compact(merge(aliases, std::slice::from_ref(&canonical)));
        for group in previous.values() {
            inner.remove_group(group);
        }
        inner.set_group(Group {
            canonical: canonical.clone(),
            expires: now + ttl,
            aliases,
        });
        inner.enforce_limit();
        canonical
    }
}

/// Go `isHierarchyParent`: `fallback` names the parent session of `primary` (an agent
/// session, or a session of the same `kind:`) rather than another name for it.
pub fn is_hierarchy_parent(primary: &str, fallback: &str) -> bool {
    if fallback.is_empty() || primary.is_empty() || primary == fallback {
        return false;
    }
    if primary.contains(":agent:") {
        return true;
    }
    match (primary.find(':'), fallback.find(':')) {
        (Some(i), Some(j)) if i > 0 && j > 0 && primary[..i] == fallback[..j] => true,
        (None, None) => true,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    /// Go's alias cache and `isHierarchyParent` on the same inputs
    /// (sdk/cliproxy/auth/zz_rustgolden_alias_test.go in the reference).
    #[test]
    fn aliases_match_go() {
        let golden: Value =
            serde_json::from_str(include_str!("../tests/fixtures/go_home_session_aliases.json")).unwrap();
        let base = Instant::now();
        for scenario in golden["scenarios"].as_array().unwrap() {
            let name = scenario["name"].as_str().unwrap();
            let cache = SessionAliases::default();
            let ttl = Duration::from_secs(scenario["ttl_secs"].as_u64().unwrap());
            for (i, call) in scenario["calls"].as_array().unwrap().iter().enumerate() {
                let at = base + Duration::from_secs(call["at_secs"].as_u64().unwrap());
                let got = cache.canonical(
                    call["primary"].as_str().unwrap(),
                    call["fallback"].as_str().unwrap(),
                    ttl,
                    at,
                );
                assert_eq!(got, call["want"].as_str().unwrap(), "{name} call {i}");
            }
        }
        for case in golden["hierarchy"].as_array().unwrap() {
            let (primary, fallback) = (case["primary"].as_str().unwrap(), case["fallback"].as_str().unwrap());
            assert_eq!(
                is_hierarchy_parent(primary, fallback),
                case["want"].as_bool().unwrap(),
                "{primary:?} {fallback:?}"
            );
        }
    }

    const MINUTE: Duration = Duration::from_secs(60);

    fn entries(cache: &SessionAliases) -> HashMap<String, Group> {
        cache.0.lock().unwrap().entries.clone()
    }

    /// Go `TestHomeSessionAliasCachePrimaryAccessRefreshesWholeAliasGroup`.
    #[test]
    fn primary_access_refreshes_the_whole_group() {
        let cache = SessionAliases::default();
        let now = Instant::now();
        let (primary, fallback) = ("pck:shared-cache-bucket", "conv:conversation-session");
        assert_eq!(cache.canonical(primary, fallback, MINUTE, now), primary);
        cache.0.lock().unwrap().entries.get_mut(fallback).unwrap().expires = now;
        let later = |s: u64| now + Duration::from_secs(s);
        assert_eq!(cache.canonical(primary, "", MINUTE, later(10)), primary);
        assert_eq!(cache.canonical(fallback, "", MINUTE, later(20)), primary);
    }

    /// Go `TestHomeSessionAliasCacheSharedPromptKeyPreservesConversationAliases` and
    /// `TestHomeSessionAliasCacheConversationIDContainingPromptMarkerRemainsStable`.
    #[test]
    fn a_shared_prompt_key_keeps_every_conversation() {
        let cache = SessionAliases::default();
        let now = Instant::now();
        let at = |s: u64| now + Duration::from_secs(s);
        let key = "pck:shared-cache-bucket";
        assert_eq!(cache.canonical(key, "conv:conversation-a", MINUTE, at(0)), key);
        assert_eq!(cache.canonical(key, "conv:conversation-b", MINUTE, at(1)), key);
        assert_eq!(cache.canonical("conv:conversation-a", "", MINUTE, at(2)), key);
        assert_eq!(cache.canonical("conv:conversation-b", "", MINUTE, at(3)), key);

        let cache = SessionAliases::default();
        assert_eq!(cache.canonical(key, "conv:a::pck:b", MINUTE, at(0)), key);
        assert_eq!(cache.canonical("conv:a::pck:b", "", MINUTE, at(1)), key);
    }

    /// Go `TestHomeSessionAliasCacheSharedPromptKeyCapsStableAliasesByRecency`.
    #[test]
    fn a_shared_prompt_key_keeps_the_newest_aliases() {
        let cache = SessionAliases::default();
        let now = Instant::now();
        for i in 0..128u64 {
            cache.canonical(
                "pck:shared-cache-bucket",
                &format!("conv:conversation-{i:03}"),
                MINUTE,
                now + Duration::from_secs(i),
            );
        }
        let entries = entries(&cache);
        assert!(entries.len() <= 65, "{}", entries.len());
        assert!(entries.contains_key("conv:conversation-127"));
        assert!(!entries.contains_key("conv:conversation-000"));
    }

    /// Go `TestHomeSessionAliasCacheRotatingPrimaryEvictsObsoleteAliases`.
    #[test]
    fn a_rotating_primary_evicts_obsolete_aliases() {
        let cache = SessionAliases::default();
        let now = Instant::now();
        let fallback = "conv:conversation-session";
        for i in 0..16u64 {
            let primary = format!("pck:cache-{i:02}");
            assert_eq!(
                cache.canonical(&primary, fallback, MINUTE, now + Duration::from_secs(i)),
                "pck:cache-00"
            );
        }
        let entries = entries(&cache);
        assert_eq!(entries.len(), 2, "{:?}", entries.keys());
        assert!(entries.contains_key("pck:cache-15") && entries.contains_key(fallback));
        assert!(!entries.contains_key("pck:cache-00"));
        assert_eq!(entries[fallback].aliases.len(), 2);
    }

    /// Go `TestHomeSessionAliasCacheDoesNotReconnectCompactedCanonicalAlias`.
    #[test]
    fn a_compacted_canonical_alias_is_not_reconnected() {
        let cache = SessionAliases::default();
        let now = Instant::now();
        let at = |s: u64| now + Duration::from_secs(s);
        let (obsolete, current, conversation) = ("pck:cache-a", "pck:cache-b", "conv:conversation-session");
        assert_eq!(cache.canonical(obsolete, conversation, MINUTE, at(0)), obsolete);
        assert_eq!(cache.canonical(current, conversation, MINUTE, at(1)), obsolete);
        assert!(!entries(&cache).contains_key(obsolete));
        assert_eq!(cache.canonical(obsolete, "", MINUTE, at(2)), obsolete);
        let entries = entries(&cache);
        assert!(!entries.contains_key(obsolete), "the old alias stays out of the group");
        assert_eq!(entries[conversation].canonical, entries[current].canonical);
        assert_eq!(cache.canonical(conversation, "", MINUTE, at(3)), obsolete);
    }

    /// Go `TestHomeSessionAliasCacheSoftLimitEvictsOldestTouchedGroup` and
    /// `TestHomeSessionAliasCacheEnforcesSoftLimit`.
    #[test]
    fn the_soft_limit_evicts_the_oldest_groups() {
        let cache = SessionAliases::default();
        let now = Instant::now();
        let hour = Duration::from_secs(3600);
        cache.canonical("session:zzzz-oldest", "", hour, now);
        for i in 0..SOFT_LIMIT {
            cache.canonical(&format!("session:{i:05}"), "", hour, now);
        }
        let entries = entries(&cache);
        assert!(entries.len() <= SOFT_LIMIT);
        assert!(!entries.contains_key("session:zzzz-oldest"));
        assert!(entries.contains_key("session:00000"));

        let cache = SessionAliases::default();
        for i in 0..SOFT_LIMIT + 32 {
            cache.canonical(
                &format!("session:{i:05}"),
                "",
                hour,
                now + Duration::from_nanos(i as u64),
            );
        }
        let entries = self::entries(&cache);
        assert!(entries.len() <= SOFT_LIMIT);
        assert!(!entries.contains_key("session:00000"));
        assert!(entries.contains_key(&format!("session:{:05}", SOFT_LIMIT + 31)));
    }

    /// Go `TestHomeSessionAliasCacheClearsWhenConfiguredTTLChanges` (at the cache: the
    /// manager passes the configured session-affinity TTL).
    #[test]
    fn a_changed_ttl_starts_over() {
        let cache = SessionAliases::default();
        let now = Instant::now();
        assert_eq!(cache.canonical("s", "pck:k", Duration::from_secs(60), now), "s");
        assert_eq!(cache.canonical("pck:k", "", Duration::from_secs(60), now), "s");
        assert_eq!(cache.canonical("pck:k", "", Duration::from_secs(120), now), "pck:k");
    }
}
