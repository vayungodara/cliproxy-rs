//! Background refresh backoff. Instants are supplied so tests never sleep.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use cpa_core::credential::Credential;
use cpa_core::exec::ExecError;

struct Entry {
    revision: u64,
    next: Instant,
    invalid_grants: u32,
    /// The `next` a log line already announced, so a hold is reported once.
    noted: Option<Instant>,
}

#[derive(Default)]
pub(crate) struct RefreshState {
    entries: HashMap<String, Entry>,
}

impl RefreshState {
    pub fn reserve(&mut self, c: &Credential, now: Instant) -> bool {
        let fresh = || Entry {
            revision: c.revision,
            next: now,
            invalid_grants: 0,
            noted: None,
        };
        let entry = self.entries.entry(c.id.clone()).or_insert_with(fresh);
        if entry.revision != c.revision {
            *entry = fresh();
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
            // Go `isInvalidGrantError`.
            if crate::classify::is_invalid_grant(error.status, &String::from_utf8_lossy(&error.body)) {
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
        // A finished job's backoff is not a hold to report (`note_hold`).
        entry.noted = Some(entry.next);
    }

    /// For a due credential that `reserve` held back: when it may be tried again, the
    /// first time a hold ending then is asked about (later scans log nothing).
    pub fn note_hold(&mut self, c: &Credential, now: Instant) -> Option<Instant> {
        let entry = self.entries.get_mut(&c.id).filter(|e| e.next > now)?;
        (entry.noted != Some(entry.next)).then(|| {
            entry.noted = Some(entry.next);
            entry.next
        })
    }

    /// When a reserved or backed-off credential may be tried again.
    pub fn retry_at(&self, c: &Credential) -> Option<Instant> {
        self.entries.get(&c.id).map(|e| e.next)
    }

    pub fn reconcile(&mut self, credentials: &[std::sync::Arc<Credential>]) {
        let ids: std::collections::HashSet<&str> = credentials.iter().map(|c| c.id.as_str()).collect();
        self.entries.retain(|id, _| ids.contains(id.as_str()));
    }

    /// Ends the hold on `c` now, as if its backoff had run out.
    #[cfg(test)]
    pub fn expire(&mut self, c: &Credential) {
        if let Some(entry) = self.entries.get_mut(&c.id) {
            entry.next = Instant::now();
        }
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

    /// A due credential held by a reservation no job released is reported once per
    /// hold; the backoff a finished job set is not reported again.
    #[test]
    fn a_hold_is_reported_once() {
        let c = Credential::from_file(
            Path::new("/mock"),
            Path::new("/mock/a"),
            serde_json::from_str(r#"{"type":"claude"}"#).unwrap(),
        )
        .unwrap();
        let now = Instant::now();
        let mut s = RefreshState::default();
        assert!(s.reserve(&c, now));
        let later = now + Duration::from_secs(1);
        assert_eq!(s.note_hold(&c, later), Some(now + Duration::from_secs(60)));
        assert_eq!(s.note_hold(&c, later), None, "once per hold");
        let error = ExecError::local(500, FailureScope::Transport, "mock failure");
        s.finish(&c, Some(&error), false, later);
        assert_eq!(s.note_hold(&c, later), None, "the failure announced its own retry");
        let retry = later + Duration::from_secs(300);
        assert!(s.reserve(&c, retry));
        assert!(s.note_hold(&c, retry + Duration::from_secs(1)).is_some(), "a new hold");
    }
}
