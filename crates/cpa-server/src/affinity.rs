//! Session-to-credential bindings with alias groups (Go sdk/cliproxy/auth/session_cache.go).
//!
//! A group binds several session keys (a session and its parent or conversation alias)
//! to one credential with one deadline. Refreshing any key refreshes the group; a failed
//! key leaves the group alone and only drops itself.

use std::collections::{BTreeMap, HashMap};
use std::time::{Duration, Instant};

/// `(provider scope, canonical model, session ID)`: Go `provider::session::model`.
pub type Key = (String, String, String);

const MAX_ENTRIES: usize = 65536;
const MAX_STABLE_ALIASES: usize = 64;

struct Group {
    auth: String,
    deadline: Instant,
    aliases: Vec<Key>,
}

#[derive(Default)]
pub struct Cache {
    entries: HashMap<Key, u64>,
    /// Groups in creation order: the smallest ID is evicted first.
    groups: BTreeMap<u64, Group>,
    /// Group ID by its first alias (Go keys groups by primary alias).
    primaries: HashMap<Key, u64>,
    next: u64,
    /// Next expired-group sweep (Go's cleanup loop runs every ttl/2).
    sweep_at: Option<Instant>,
}

impl Cache {
    fn live(&self, key: &Key, now: Instant) -> Option<(u64, &Group)> {
        let id = *self.entries.get(key)?;
        let group = self.groups.get(&id)?;
        (now < group.deadline).then_some((id, group))
    }

    /// Go `Get`: the bound credential, without refreshing.
    pub fn get(&mut self, key: &Key, now: Instant) -> Option<String> {
        let id = *self.entries.get(key)?;
        match self.live(key, now) {
            Some((_, group)) => Some(group.auth.clone()),
            None => {
                self.remove(id);
                None
            }
        }
    }

    /// The credentials bound to live keys that satisfy `matches`, without refreshing
    /// (Go `Get` over each candidate key, for `LookupAffinity`).
    pub fn bound(&self, now: Instant, matches: impl Fn(&Key) -> bool) -> Vec<String> {
        self.entries
            .keys()
            .filter(|key| matches(key))
            .filter_map(|key| self.live(key, now).map(|(_, group)| group.auth.clone()))
            .collect()
    }

    /// Go `GetAndRefresh`: the bound credential, extending its group.
    pub fn get_and_refresh(&mut self, key: &Key, now: Instant, ttl: Duration) -> Option<String> {
        let id = *self.entries.get(key)?;
        let Some((_, group)) = self.live(key, now) else {
            self.remove(id);
            return None;
        };
        let auth = group.auth.clone();
        let aliases = compact(merge(vec![key.clone()], &group.aliases));
        self.replace(&auth, now + ttl, aliases, &[id]);
        Some(auth)
    }

    /// Go `SetAliases`: binds `keys` (and the live groups they belong to) to `auth`.
    pub fn bind(&mut self, auth: &str, keys: &[Key], now: Instant, ttl: Duration) {
        let mut aliases = merge(Vec::new(), keys);
        let mut previous = Vec::new();
        for key in keys {
            let Some(&id) = self.entries.get(key) else {
                continue;
            };
            match self.live(key, now) {
                Some((_, group)) => {
                    aliases = merge(aliases, &group.aliases);
                    previous.push(id);
                }
                None => self.remove(id),
            }
        }
        let aliases = compact(aliases);
        if !aliases.is_empty() {
            self.replace(auth, now + ttl, aliases, &previous);
        }
    }

    /// Go `Touch`: refreshes `key`'s group if it is still bound to `auth`.
    pub fn touch(&mut self, key: &Key, auth: &str, now: Instant, ttl: Duration) {
        let Some((id, group)) = self.live(key, now) else {
            return;
        };
        if group.auth != auth {
            return;
        }
        let aliases = compact(merge(vec![key.clone()], &group.aliases));
        self.replace(auth, now + ttl, aliases, &[id]);
    }

    /// Go `CompareAndDelete`: drops `key` from its group if bound to `auth`.
    pub fn compare_and_delete(&mut self, key: &Key, auth: &str) {
        let Some(&id) = self.entries.get(key) else {
            return;
        };
        let Some(group) = self.groups.get(&id) else {
            return;
        };
        if group.auth != auth {
            return;
        }
        let deadline = group.deadline;
        let surviving: Vec<Key> = group.aliases.iter().filter(|a| *a != key).cloned().collect();
        self.remove(id);
        if !surviving.is_empty() {
            self.replace(auth, deadline, surviving, &[]);
        }
    }

    /// Go `InvalidateAuth`: removes every group bound to `auth`.
    pub fn invalidate(&mut self, auth: &str) {
        let ids: Vec<u64> = self
            .groups
            .iter()
            .filter(|(_, g)| g.auth == auth)
            .map(|(id, _)| *id)
            .collect();
        ids.into_iter().for_each(|id| self.remove(id));
    }

    /// Drops expired groups at most every `ttl / 2`.
    pub fn sweep(&mut self, now: Instant, ttl: Duration) {
        if self.sweep_at.is_some_and(|at| now < at) {
            return;
        }
        self.sweep_at = Some(now + (ttl / 2).max(Duration::from_millis(1)));
        let expired: Vec<u64> = self
            .groups
            .iter()
            .filter(|(_, g)| now >= g.deadline)
            .map(|(id, _)| *id)
            .collect();
        expired.into_iter().for_each(|id| self.remove(id));
    }

    /// Removes groups bound to credentials `keep` rejects.
    pub fn retain(&mut self, keep: impl Fn(&str) -> bool) {
        let ids: Vec<u64> = self
            .groups
            .iter()
            .filter(|(_, g)| !keep(&g.auth))
            .map(|(id, _)| *id)
            .collect();
        ids.into_iter().for_each(|id| self.remove(id));
    }

    pub fn clear(&mut self) {
        *self = Self::default();
    }

    #[cfg(test)]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    fn remove(&mut self, id: u64) {
        let Some(group) = self.groups.remove(&id) else {
            return;
        };
        if self.primaries.get(&group.aliases[0]) == Some(&id) {
            self.primaries.remove(&group.aliases[0]);
        }
        for alias in &group.aliases {
            if self.entries.get(alias) == Some(&id) {
                self.entries.remove(alias);
            }
        }
    }

    /// Go `replaceAliasGroupsLocked`.
    fn replace(&mut self, auth: &str, deadline: Instant, aliases: Vec<Key>, previous: &[u64]) {
        previous.iter().for_each(|id| self.remove(*id));
        if let Some(&existing) = self.primaries.get(&aliases[0]) {
            self.remove(existing);
        }
        let id = self.next;
        self.next += 1;
        for alias in &aliases {
            self.entries.insert(alias.clone(), id);
        }
        self.primaries.insert(aliases[0].clone(), id);
        self.groups.insert(
            id,
            Group {
                auth: auth.to_owned(),
                deadline,
                aliases,
            },
        );
        while self.entries.len() > MAX_ENTRIES {
            let Some(oldest) = self.groups.keys().next().copied() else {
                break;
            };
            self.remove(oldest);
        }
    }
}

/// Go `mergeSessionAliases`: order-preserving union without empty sessions.
fn merge(mut out: Vec<Key>, more: &[Key]) -> Vec<Key> {
    let mut deduped: Vec<Key> = Vec::with_capacity(out.len() + more.len());
    for key in out.drain(..).chain(more.iter().cloned()) {
        if !key.2.is_empty() && !deduped.contains(&key) {
            deduped.push(key);
        }
    }
    deduped
}

/// Go `compactSessionAliases`: one prompt-cache alias and at most 64 others.
fn compact(aliases: Vec<Key>) -> Vec<Key> {
    let mut prompt_cache = false;
    let mut stable = 0;
    aliases
        .into_iter()
        .filter(|key| {
            if key.2.starts_with("pck:") {
                !std::mem::replace(&mut prompt_cache, true)
            } else {
                stable += 1;
                stable <= MAX_STABLE_ALIASES
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(session: &str) -> Key {
        ("claude".into(), "m".into(), session.into())
    }

    #[test]
    fn groups_refresh_together_and_failures_drop_only_their_key() {
        let ttl = Duration::from_secs(10);
        let t0 = Instant::now();
        let mut cache = Cache::default();
        cache.bind("a", &[key("pck:1"), key("conv:1")], t0, ttl);
        // Refreshing one alias extends the whole group.
        let t1 = t0 + Duration::from_secs(8);
        assert_eq!(cache.get_and_refresh(&key("pck:1"), t1, ttl).as_deref(), Some("a"));
        let t2 = t0 + Duration::from_secs(15);
        assert_eq!(cache.get(&key("conv:1"), t2).as_deref(), Some("a"));
        // A failure on the parent alias leaves the child bound, same deadline.
        cache.compare_and_delete(&key("conv:1"), "a");
        assert_eq!(cache.get(&key("conv:1"), t2), None);
        assert_eq!(cache.get(&key("pck:1"), t2).as_deref(), Some("a"));
        assert_eq!(cache.get(&key("pck:1"), t1 + ttl), None, "deadline kept, not refreshed");
        // A different credential's failure changes nothing.
        cache.bind("a", &[key("s")], t0, ttl);
        cache.compare_and_delete(&key("s"), "b");
        assert_eq!(cache.get(&key("s"), t0).as_deref(), Some("a"));
        // Touch only refreshes the current owner.
        cache.touch(&key("s"), "b", t0 + Duration::from_secs(9), ttl);
        assert_eq!(cache.get(&key("s"), t0 + Duration::from_secs(11)), None);
    }

    #[test]
    fn binding_merges_existing_groups_and_keeps_one_prompt_cache_alias() {
        let ttl = Duration::from_secs(10);
        let t0 = Instant::now();
        let mut cache = Cache::default();
        cache.bind("a", &[key("pck:1"), key("conv:1")], t0, ttl);
        // A new prompt-cache key on the same conversation joins the group; the older
        // prompt-cache alias is compacted away.
        cache.bind("b", &[key("pck:2"), key("conv:1")], t0, ttl);
        assert_eq!(cache.get(&key("pck:2"), t0).as_deref(), Some("b"));
        assert_eq!(cache.get(&key("conv:1"), t0).as_deref(), Some("b"));
        assert_eq!(cache.get(&key("pck:1"), t0), None);
        cache.invalidate("b");
        assert!(cache.is_empty());
    }

    #[test]
    fn eviction_drops_oldest_groups_past_capacity() {
        let ttl = Duration::from_secs(10);
        let t0 = Instant::now();
        let mut cache = Cache::default();
        for i in 0..=MAX_ENTRIES {
            cache.bind("a", &[key(&format!("s{i}"))], t0, ttl);
        }
        assert_eq!(cache.entries.len(), MAX_ENTRIES);
        assert_eq!(cache.get(&key("s0"), t0), None);
        assert_eq!(cache.get(&key("s1"), t0).as_deref(), Some("a"));
    }
}
