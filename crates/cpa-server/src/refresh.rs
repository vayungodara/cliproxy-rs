//! Background refresh backoff. Instants are supplied so tests never sleep.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use cpa_core::credential::Credential;
use cpa_core::exec::ExecError;

struct Entry {
    revision: u64,
    next: Instant,
    invalid_grants: u32,
}

#[derive(Default)]
pub(crate) struct RefreshState {
    entries: HashMap<String, Entry>,
}

impl RefreshState {
    pub fn reserve(&mut self, c: &Credential, now: Instant) -> bool {
        let entry = self.entries.entry(c.id.clone()).or_insert(Entry {
            revision: c.revision,
            next: now,
            invalid_grants: 0,
        });
        if entry.revision != c.revision {
            *entry = Entry {
                revision: c.revision,
                next: now,
                invalid_grants: 0,
            };
        }
        if c.disabled || entry.next > now {
            return false;
        }
        entry.next = now + Duration::from_secs(60);
        true
    }

    pub fn finish(&mut self, c: &Credential, error: Option<&ExecError>, ineffective: bool, now: Instant) {
        let Some(entry) = self.entries.get_mut(&c.id) else {
            return;
        };
        entry.revision = c.revision;
        let seconds = if let Some(error) = error {
            if String::from_utf8_lossy(&error.body)
                .to_ascii_lowercase()
                .contains("invalid_grant")
            {
                let seconds = (60u64 << entry.invalid_grants.min(5)).min(1800);
                entry.invalid_grants += u32::from(seconds < 1800);
                seconds
            } else {
                entry.invalid_grants = 0;
                300
            }
        } else {
            entry.invalid_grants = 0;
            if ineffective { 30 } else { 0 }
        };
        entry.next = now + Duration::from_secs(seconds);
    }

    /// When a reserved or backed-off credential may be tried again.
    pub fn retry_at(&self, c: &Credential) -> Option<Instant> {
        self.entries.get(&c.id).map(|e| e.next)
    }

    pub fn reconcile(&mut self, credentials: &[std::sync::Arc<Credential>]) {
        let ids: std::collections::HashSet<&str> = credentials.iter().map(|c| c.id.as_str()).collect();
        self.entries.retain(|id, _| ids.contains(id.as_str()));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cpa_core::exec::FailureScope;
    use std::path::Path;

    #[test]
    fn pending_failure_ineffective_rotation_and_invalid_grant_backoffs() {
        let mut c = Credential::from_file(
            Path::new("/mock"),
            Path::new("/mock/a"),
            serde_json::from_str(r#"{"type":"claude"}"#).unwrap(),
        )
        .unwrap();
        c.revision = 1;
        let now = Instant::now();
        let mut s = RefreshState::default();
        assert!(s.reserve(&c, now));
        assert!(!s.reserve(&c, now + Duration::from_secs(59)));
        assert!(s.reserve(&c, now + Duration::from_secs(60)));
        let error = ExecError::local(500, FailureScope::Transport, "mock failure");
        s.finish(&c, Some(&error), false, now);
        assert!(!s.reserve(&c, now + Duration::from_secs(299)));
        assert!(s.reserve(&c, now + Duration::from_secs(300)));
        c.revision = 2;
        s.finish(&c, None, true, now);
        assert!(!s.reserve(&c, now + Duration::from_secs(29)));
        assert!(s.reserve(&c, now + Duration::from_secs(30)));
        let error = ExecError::local(400, FailureScope::Credential, "invalid_grant");
        let mut t = now;
        for delay in [60, 120, 240, 480, 960, 1800, 1800] {
            s.finish(&c, Some(&error), false, t);
            assert!(!s.reserve(&c, t + Duration::from_secs(delay - 1)));
            t += Duration::from_secs(delay);
            assert!(s.reserve(&c, t));
        }
        c.revision = 3;
        assert!(s.reserve(&c, t), "management edit invalidates backoff");
        c.disabled = true;
        c.revision = 4;
        assert!(!s.reserve(&c, t), "disabled credentials never refresh");
        s.reconcile(&[]);
        assert!(s.entries.is_empty(), "deletion drops refresh state");
    }
}
