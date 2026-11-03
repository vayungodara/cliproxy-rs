//! Background refresh backoff. Instants are supplied so tests never sleep.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use cpa_core::credential::Credential;
use cpa_core::exec::ExecError;

struct Entry {
    revision: u64,
    next: Instant,
    invalid_grants: u32,
    /// A job reserved by `reserve` has not ended yet (Go keeps one refresh job per
    /// auth, `markRefreshPending`). No other job starts meanwhile, whatever `next` says.
    running: bool,
    /// Whether the running job's hold was already logged (`note_hold`).
    noted: bool,
}

#[derive(Default)]
pub(crate) struct RefreshState {
    entries: HashMap<String, Entry>,
    /// The revision of each disabled credential whose skipped due refresh was logged.
    disabled_noted: HashMap<String, u64>,
}

impl RefreshState {
    /// Reserves a due credential for one job, unless a job for it is still running or
    /// it is backing off. The job ends with [`Self::finish`] or [`Self::release`].
    pub fn reserve(&mut self, c: &Credential, now: Instant) -> bool {
        let fresh = |running| Entry {
            revision: c.revision,
            next: now,
            invalid_grants: 0,
            running,
            noted: false,
        };
        let entry = self.entries.entry(c.id.clone()).or_insert_with(|| fresh(false));
        if entry.revision != c.revision {
            *entry = fresh(entry.running);
        }
        if c.disabled || entry.running || entry.next > now {
            return false;
        }
        entry.next = now + Duration::from_secs(60);
        entry.running = true;
        entry.noted = false;
        true
    }

    /// Ends the job on `c` and sets when the next one may start.
    pub fn finish(&mut self, c: &Credential, error: Option<&ExecError>, ineffective: bool, now: Instant) {
        let Some(entry) = self.entries.get_mut(&c.id) else {
            return;
        };
        entry.revision = c.revision;
        entry.running = false;
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
        entry.next = now + backoff_override(&c.id).unwrap_or(Duration::from_secs(seconds));
    }

    /// Ends the job on credential `id` with nothing to record (the credential was
    /// removed or replaced meanwhile).
    pub fn release(&mut self, id: &str) {
        if let Some(entry) = self.entries.get_mut(id) {
            entry.running = false;
        }
    }

    /// Whether a due credential is held by a job still running, true once per job:
    /// later scans log nothing.
    pub fn note_hold(&mut self, c: &Credential) -> bool {
        self.entries
            .get_mut(&c.id)
            .filter(|e| e.running && !e.noted)
            .is_some_and(|e| {
                e.noted = true;
                true
            })
    }

    /// Records that a due refresh of disabled `c` was logged at this revision.
    pub fn note_disabled(&mut self, c: &Credential) {
        self.disabled_noted.insert(c.id.clone(), c.revision);
    }

    /// Whether a due refresh of disabled `c` was already logged at this revision.
    pub fn disabled_noted(&self, c: &Credential) -> bool {
        self.disabled_noted.get(&c.id) == Some(&c.revision)
    }

    /// When a reserved or backed-off credential may be tried again.
    pub fn retry_at(&self, c: &Credential) -> Option<Instant> {
        self.entries.get(&c.id).map(|e| e.next)
    }

    /// Drops the state of removed credentials, except a job still running: it ends
    /// first, so a credential added again under the ID waits for it.
    pub fn reconcile(&mut self, credentials: &[std::sync::Arc<Credential>]) {
        let ids: std::collections::HashSet<&str> = credentials.iter().map(|c| c.id.as_str()).collect();
        self.entries.retain(|id, e| e.running || ids.contains(id.as_str()));
        self.disabled_noted.retain(|id, _| ids.contains(id.as_str()));
    }

    /// Ends the hold on `c` now, as if its backoff had run out.
    #[cfg(test)]
    pub fn expire(&mut self, c: &Credential) {
        if let Some(entry) = self.entries.get_mut(&c.id) {
            entry.next = Instant::now();
        }
    }

    #[cfg(test)]
    pub fn running(&self, c: &Credential) -> bool {
        self.entries.get(&c.id).is_some_and(|e| e.running)
    }
}

/// A test's backoff for credential `id` ([`override_backoff`]); none otherwise.
fn backoff_override(id: &str) -> Option<Duration> {
    #[cfg(test)]
    if let Some((_, d)) = OVERRIDES.lock().unwrap().iter().find(|(i, _)| i == id) {
        return Some(*d);
    }
    let _ = id;
    None
}

#[cfg(test)]
static OVERRIDES: std::sync::Mutex<Vec<(String, Duration)>> = std::sync::Mutex::new(Vec::new());

/// Every outcome of credential `id` (failure, success or ineffective refresh) holds the
/// next refresh off for `d`, for tests that cannot wait minutes.
#[cfg(test)]
pub(crate) fn override_backoff(id: &str, d: Duration) {
    OVERRIDES.lock().unwrap().push((id.to_owned(), d));
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
        assert!(!s.reserve(&c, now + Duration::from_secs(60)), "the job has not ended");
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
        s.release(&c.id);
        c.revision = 3;
        assert!(s.reserve(&c, t), "management edit invalidates backoff");
        s.release(&c.id);
        c.disabled = true;
        c.revision = 4;
        assert!(!s.reserve(&c, t), "disabled credentials never refresh");
        s.reconcile(&[]);
        assert!(s.entries.is_empty(), "deletion drops refresh state");
    }

    /// A due credential held by a job still running is reported once per job; a new
    /// job starts only after it ends, even when its backoff has run out.
    #[test]
    fn one_job_at_a_time_and_its_hold_is_reported_once() {
        let c = Credential::from_file(
            Path::new("/mock"),
            Path::new("/mock/a"),
            serde_json::from_str(r#"{"type":"claude"}"#).unwrap(),
        )
        .unwrap();
        let now = Instant::now();
        let mut s = RefreshState::default();
        assert!(s.reserve(&c, now));
        let later = now + Duration::from_secs(600);
        assert!(!s.reserve(&c, later), "the first job is still running");
        assert!(s.note_hold(&c));
        assert!(!s.note_hold(&c), "once per job");
        s.reconcile(&[]);
        assert!(!s.reserve(&c, later), "a removal keeps a running job's mark");
        s.finish(&c, None, false, later);
        assert!(!s.note_hold(&c), "nothing runs");
        assert!(s.reserve(&c, later));
        assert!(s.note_hold(&c), "a new job, a new hold");
        s.release(&c.id);
        assert!(s.reserve(&c, later + Duration::from_secs(60)));
    }
}
