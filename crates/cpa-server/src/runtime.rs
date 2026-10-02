//! Axum-independent runtime: config snapshot, credential store, selection and outcomes.

use std::io::Write;
use std::path::Path;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, PoisonError, RwLock};
use std::task::{Context, Poll, ready};
use std::time::Duration;

use bytes::Bytes;
use cpa_core::config::Config;
use cpa_core::credential::{Credential, MetadataPatch, Source};
use cpa_core::exec::{ExecError, ExecStream, FailureScope};
use cpa_exec::Executors;
use futures_util::{Stream, StreamExt};
use serde_json::{Map, Value};

pub struct Runtime {
    config: RwLock<Arc<Config>>,
    store: CredentialStore,
    pub executors: Executors,
}

impl Runtime {
    pub fn new(config: Config, credentials: Vec<Credential>, executors: Executors) -> Self {
        Self {
            config: RwLock::new(Arc::new(config)),
            store: CredentialStore::new(credentials),
            executors,
        }
    }

    pub fn config(&self) -> Arc<Config> {
        self.config
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    pub fn store(&self) -> &CredentialStore {
        &self.store
    }
}

/// An owned credential snapshot for one attempt. Report how it went with
/// [`CredentialStore::complete`], or wrap the response stream in [`Completing`].
#[derive(Debug, Clone)]
pub struct Lease {
    pub credential: Arc<Credential>,
    pub attempt: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Success,
    Failure {
        scope: FailureScope,
        status: u16,
        retry_after: Option<Duration>,
    },
    /// The client went away before the response finished.
    Cancelled,
}

impl Outcome {
    pub fn from_error(e: &ExecError) -> Self {
        Outcome::Failure {
            scope: e.scope,
            status: e.status,
            retry_after: e.retry_after,
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum PatchError {
    NotFound,
    /// The credential changed since the caller read it; re-read and retry.
    Stale {
        current: u64,
    },
    Io(String),
}

pub struct CredentialStore {
    creds: RwLock<Vec<Arc<Credential>>>,
    cursor: AtomicUsize,
    attempts: AtomicU64,
}

impl CredentialStore {
    pub fn new(credentials: Vec<Credential>) -> Self {
        Self {
            creds: RwLock::new(credentials.into_iter().map(Arc::new).collect()),
            cursor: AtomicUsize::new(0),
            attempts: AtomicU64::new(0),
        }
    }

    pub fn snapshot(&self) -> Vec<Arc<Credential>> {
        self.creds
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    // ponytail: round-robin over enabled credentials of one provider. Strategies,
    // priority/weight, session affinity, model support and cooldown are the M4
    // scheduler port (sdk/cliproxy/auth/conductor*.go).
    pub fn select(&self, provider: &str) -> Option<Lease> {
        let creds = self.creds.read().unwrap_or_else(PoisonError::into_inner);
        let candidates: Vec<&Arc<Credential>> = creds
            .iter()
            .filter(|c| c.provider == provider && !c.disabled)
            .collect();
        if candidates.is_empty() {
            return None;
        }
        let i = self.cursor.fetch_add(1, Ordering::Relaxed) % candidates.len();
        Some(Lease {
            credential: candidates[i].clone(),
            attempt: self.attempts.fetch_add(1, Ordering::Relaxed),
        })
    }

    // ponytail: outcomes are only logged. Cooldown, model cooling and retry state
    // arrive with the scheduler port.
    pub fn complete(&self, lease: &Lease, outcome: Outcome) {
        tracing::debug!(credential = %lease.credential.id, attempt = lease.attempt, ?outcome, "attempt finished");
    }

    /// Applies a metadata change if the credential is still at `expected_revision`.
    /// File-backed credentials are written atomically (0600) before memory changes.
    pub fn apply_patch(
        &self,
        id: &str,
        expected_revision: u64,
        patch: &MetadataPatch,
    ) -> Result<Arc<Credential>, PatchError> {
        // ponytail: one write lock across the file write. Fine at refresh rates; per-
        // credential locks if management bulk edits ever contend with selection.
        let mut creds = self.creds.write().unwrap_or_else(PoisonError::into_inner);
        let slot = creds
            .iter_mut()
            .find(|c| c.id == id)
            .ok_or(PatchError::NotFound)?;
        if slot.revision != expected_revision {
            return Err(PatchError::Stale {
                current: slot.revision,
            });
        }
        let mut next = Credential::clone(slot);
        patch.apply(&mut next.metadata);
        next.disabled = next
            .metadata
            .get("disabled")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        next.revision += 1;
        if let Source::File(path) = &next.source {
            write_atomic(path, &next.metadata).map_err(|e| PatchError::Io(e.to_string()))?;
        }
        *slot = Arc::new(next);
        Ok(slot.clone())
    }
}

/// Same bytes Go's `json.NewEncoder(f).Encode` writes: compact JSON plus a newline.
fn write_atomic(path: &Path, metadata: &Map<String, Value>) -> std::io::Result<()> {
    use std::os::unix::fs::OpenOptionsExt;
    let mut bytes = serde_json::to_vec(metadata)?;
    bytes.push(b'\n');
    let tmp = path.with_extension(format!("tmp-{}", std::process::id()));
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&tmp)?;
    file.write_all(&bytes)?;
    file.sync_all()?;
    std::fs::rename(&tmp, path)
}

/// A response stream that reports its lease's outcome when it ends, fails, or is dropped.
pub struct Completing {
    inner: ExecStream,
    runtime: Arc<Runtime>,
    lease: Option<Lease>,
}

impl Completing {
    pub fn new(inner: ExecStream, runtime: Arc<Runtime>, lease: Lease) -> Self {
        Self {
            inner,
            runtime,
            lease: Some(lease),
        }
    }

    fn finish(&mut self, outcome: Outcome) {
        if let Some(lease) = self.lease.take() {
            self.runtime.store().complete(&lease, outcome);
        }
    }
}

impl Stream for Completing {
    type Item = Result<Bytes, ExecError>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        let item = ready!(this.inner.poll_next_unpin(cx));
        match &item {
            None => this.finish(Outcome::Success),
            Some(Err(e)) => this.finish(Outcome::from_error(e)),
            Some(Ok(_)) => {}
        }
        Poll::Ready(item)
    }
}

impl Drop for Completing {
    fn drop(&mut self) {
        self.finish(Outcome::Cancelled);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn cred(id: &str, provider: &str, disabled: bool) -> Credential {
        let mut metadata = Map::new();
        metadata.insert("type".into(), provider.into());
        metadata.insert("disabled".into(), disabled.into());
        Credential::from_file(Path::new("/a"), &Path::new("/a").join(id), metadata).unwrap()
    }

    #[test]
    fn select_round_robins_enabled_credentials_of_one_provider() {
        let store = CredentialStore::new(vec![
            cred("c1.json", "claude", false),
            cred("off.json", "claude", true),
            cred("x.json", "codex", false),
            cred("c2.json", "claude", false),
        ]);
        let picks: Vec<String> = (0..4)
            .map(|_| store.select("claude").unwrap().credential.id.clone())
            .collect();
        assert_eq!(picks, ["c1.json", "c2.json", "c1.json", "c2.json"]);
        assert!(store.select("gemini").is_none());
    }

    #[test]
    fn patch_persists_atomically_preserves_unknown_fields_and_rejects_stale() {
        let dir = std::env::temp_dir().join(format!("cpa-store-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("claude-a.json");
        std::fs::write(
            &path,
            r#"{"type":"claude","access_token":"old","zz_unknown":{"k":[1,2]}}"#,
        )
        .unwrap();
        let store = CredentialStore::new(cpa_core::credential::load_dir(&dir).unwrap());

        let mut patch = MetadataPatch::default();
        patch.set.insert("access_token".into(), "new".into());
        patch.set.insert("disabled".into(), true.into());
        let updated = store.apply_patch("claude-a.json", 0, &patch).unwrap();
        assert_eq!((updated.revision, updated.disabled), (1, true));
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "{\"type\":\"claude\",\"access_token\":\"new\",\"zz_unknown\":{\"k\":[1,2]},\"disabled\":true}\n"
        );
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert!(
            store.select("claude").is_none(),
            "patched-disabled credential must leave rotation"
        );
        assert_eq!(
            store.apply_patch("claude-a.json", 0, &patch).unwrap_err(),
            PatchError::Stale { current: 1 }
        );
        assert_eq!(
            store.apply_patch("nope.json", 0, &patch).unwrap_err(),
            PatchError::NotFound
        );
        assert_eq!(
            std::fs::read_dir(&dir).unwrap().count(),
            1,
            "no temp files left behind"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
