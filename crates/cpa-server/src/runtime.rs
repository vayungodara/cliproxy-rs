//! Axum-independent runtime: config snapshot, credential store, selection, credential
//! preparation and attempt outcomes.
//!
//! Every attempt is a [`Lease`]: an owned guard that reports exactly one [`Outcome`],
//! either explicitly or as `Cancelled` when dropped (client gone mid-execute, stream
//! dropped). Streams wrap their lease in [`Completing`].

use std::collections::HashMap;
use std::io::Write;
use std::path::Path;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError, RwLock};
use std::task::{Context, Poll, ready};

use bytes::Bytes;
use cpa_core::config::Config;
use cpa_core::credential::{Credential, MetadataPatch, Source};
use cpa_core::exec::{ExecError, ExecStream, FailureScope};
use cpa_exec::Executors;
use futures_util::{Stream, StreamExt};
use serde_json::{Map, Value};

pub struct Runtime {
    config: RwLock<Arc<Config>>,
    store: Arc<CredentialStore>,
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

    /// The config snapshot to use for one whole request.
    pub fn config(&self) -> Arc<Config> {
        self.config.read().unwrap_or_else(PoisonError::into_inner).clone()
    }

    /// Replaces the config. Requests already running keep their snapshot.
    pub fn publish_config(&self, config: Config) {
        *self.config.write().unwrap_or_else(PoisonError::into_inner) = Arc::new(config);
    }

    pub fn store(&self) -> &Arc<CredentialStore> {
        &self.store
    }

    /// Selects a credential and prepares it if its executor asks to, single-flighted per
    /// credential. The returned lease holds the committed, prepared snapshot.
    pub async fn acquire(&self, selection: Selection, cfg: &Config) -> Result<Lease, AcquireError> {
        let mut lease = self.store.select(selection).ok_or(AcquireError::NoCredential)?;
        if !self.executors.needs_prepare(&lease.credential, cfg) {
            return Ok(lease);
        }
        let id = lease.credential.id.clone();
        let lock = self.store.prepare_lock(&id);
        let _guard = lock.lock().await;
        // Another request may have prepared it while we waited.
        let current = self.store.get(&id).ok_or(AcquireError::NoCredential)?;
        if self.executors.needs_prepare(&current, cfg) {
            let patch = match self.executors.prepare(&current, cfg).await {
                Ok(patch) => patch,
                Err(e) => {
                    lease.complete(Outcome::Failure(e.clone()));
                    return Err(AcquireError::Prepare(e));
                }
            };
            let store = self.store.clone();
            let revision = current.revision;
            let committed = tokio::task::spawn_blocking(move || store.apply_patch(&id, revision, &patch))
                .await
                .map_err(|e| PatchError::Io(e.to_string()))
                .and_then(|r| r);
            match committed {
                Ok(cred) => lease.credential = cred,
                Err(e) => {
                    let e = ExecError::local(
                        500,
                        FailureScope::Credential,
                        format!("committing prepared credential: {e:?}"),
                    );
                    lease.complete(Outcome::Failure(e.clone()));
                    return Err(AcquireError::Prepare(e));
                }
            }
        } else {
            lease.credential = current;
        }
        Ok(lease)
    }
}

#[derive(Debug)]
pub enum AcquireError {
    NoCredential,
    Prepare(ExecError),
}

/// What the caller is asking for. The first scheduler ignores everything but
/// `provider` and `exclude`; the fields exist so the scheduler port does not change the
/// signature.
#[derive(Debug, Clone, Default)]
pub struct Selection {
    pub provider: String,
    pub model: String,
    pub session: Option<String>,
    /// Credential IDs already tried in this request.
    pub exclude: Vec<String>,
}

#[derive(Debug, Clone)]
pub enum Outcome {
    Success,
    Failure(ExecError),
    /// The client went away before the response finished.
    Cancelled,
}

/// One attempt with one credential. Not cloneable: it reports exactly one outcome.
pub struct Lease {
    store: Arc<CredentialStore>,
    pub credential: Arc<Credential>,
    pub selection: Selection,
    pub attempt: u64,
    reported: bool,
}

impl Lease {
    pub fn complete(mut self, outcome: Outcome) {
        self.report(outcome);
    }

    fn report(&mut self, outcome: Outcome) {
        if std::mem::replace(&mut self.reported, true) {
            return;
        }
        self.store.record(self, &outcome);
    }
}

impl Drop for Lease {
    fn drop(&mut self) {
        self.report(Outcome::Cancelled);
    }
}

impl std::fmt::Debug for Lease {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Lease")
            .field("credential", &self.credential.id)
            .field("attempt", &self.attempt)
            .finish()
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum PatchError {
    NotFound,
    /// The credential changed since the caller read it; re-read and retry.
    Stale {
        current: u64,
    },
    /// `type` decides the executor and cannot change in place.
    TypeIsImmutable,
    /// Config-sourced credentials persist through the config document, not here.
    ConfigBacked,
    Io(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct AttemptStats {
    pub success: u64,
    pub failure: u64,
    pub cancelled: u64,
}

struct Inner {
    creds: Vec<Arc<Credential>>,
    /// Last revision handed out. Revisions are never reused.
    generation: u64,
}

pub struct CredentialStore {
    inner: RwLock<Inner>,
    cursor: AtomicUsize,
    attempts: AtomicU64,
    stats: [AtomicU64; 3],
    prepare_locks: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
}

impl CredentialStore {
    pub fn new(credentials: Vec<Credential>) -> Arc<Self> {
        let mut inner = Inner {
            creds: Vec::new(),
            generation: 0,
        };
        for mut cred in credentials {
            inner.generation += 1;
            cred.revision = inner.generation;
            inner.creds.push(Arc::new(cred));
        }
        Arc::new(Self {
            inner: RwLock::new(inner),
            cursor: AtomicUsize::new(0),
            attempts: AtomicU64::new(0),
            stats: Default::default(),
            prepare_locks: Mutex::default(),
        })
    }

    pub fn snapshot(&self) -> Vec<Arc<Credential>> {
        self.read().creds.clone()
    }

    pub fn get(&self, id: &str) -> Option<Arc<Credential>> {
        self.read().creds.iter().find(|c| c.id == id).cloned()
    }

    // ponytail: round-robin over enabled credentials of one provider. Strategies,
    // priority/weight, session affinity, model support and cooldown are the M4
    // scheduler port (sdk/cliproxy/auth/conductor*.go).
    pub fn select(self: &Arc<Self>, selection: Selection) -> Option<Lease> {
        let credential = {
            let inner = self.read();
            let candidates: Vec<&Arc<Credential>> = inner
                .creds
                .iter()
                .filter(|c| c.provider == selection.provider && !c.disabled && !selection.exclude.contains(&c.id))
                .collect();
            if candidates.is_empty() {
                return None;
            }
            candidates[self.cursor.fetch_add(1, Ordering::Relaxed) % candidates.len()].clone()
        };
        Some(Lease {
            store: self.clone(),
            credential,
            selection,
            attempt: self.attempts.fetch_add(1, Ordering::Relaxed),
            reported: false,
        })
    }

    // ponytail: outcomes are only logged. Cooldown, model cooling and retry state
    // arrive with the scheduler port; the lease carries the model and session they need.
    fn record(&self, lease: &Lease, outcome: &Outcome) {
        let slot = match outcome {
            Outcome::Success => 0,
            Outcome::Failure(_) => 1,
            Outcome::Cancelled => 2,
        };
        self.stats[slot].fetch_add(1, Ordering::Relaxed);
        tracing::debug!(
            credential = %lease.credential.id,
            model = %lease.selection.model,
            attempt = lease.attempt,
            ?outcome,
            "attempt finished"
        );
    }

    /// Attempts finished so far, by outcome.
    pub fn stats(&self) -> AttemptStats {
        let get = |i: usize| self.stats[i].load(Ordering::Relaxed);
        AttemptStats {
            success: get(0),
            failure: get(1),
            cancelled: get(2),
        }
    }

    /// Serializes preparation of one credential across concurrent requests.
    pub fn prepare_lock(&self, id: &str) -> Arc<tokio::sync::Mutex<()>> {
        let mut locks = self.prepare_locks.lock().unwrap_or_else(PoisonError::into_inner);
        locks.entry(id.to_owned()).or_default().clone()
    }

    /// Applies a metadata change if the credential is still at `expected_revision`.
    /// File-backed credentials are written atomically (0600) before memory changes.
    /// Blocking: call from `spawn_blocking` in async code.
    pub fn apply_patch(
        &self,
        id: &str,
        expected_revision: u64,
        patch: &MetadataPatch,
    ) -> Result<Arc<Credential>, PatchError> {
        let mut inner = self.inner.write().unwrap_or_else(PoisonError::into_inner);
        let generation = inner.generation + 1;
        let slot = inner
            .creds
            .iter_mut()
            .find(|c| c.id == id)
            .ok_or(PatchError::NotFound)?;
        if slot.revision != expected_revision {
            return Err(PatchError::Stale { current: slot.revision });
        }
        if patch.remove.iter().any(|k| k == "type")
            || patch
                .set
                .get("type")
                .is_some_and(|t| t.as_str() != Some(slot.provider.as_str()))
        {
            return Err(PatchError::TypeIsImmutable);
        }
        let Source::File(path) = &slot.source else {
            return Err(PatchError::ConfigBacked);
        };
        let mut next = Credential::clone(slot);
        patch.apply(&mut next.metadata);
        next.refresh_derived();
        next.revision = generation;
        write_atomic(path, &next.metadata).map_err(|e| PatchError::Io(e.to_string()))?;
        *slot = Arc::new(next);
        let committed = slot.clone();
        inner.generation = generation;
        Ok(committed)
    }

    /// Replaces the credential set (watcher reload, management import/delete). Unchanged
    /// credentials keep their revision; new and changed ones get fresh revisions.
    pub fn reconcile(&self, credentials: Vec<Credential>) {
        let mut inner = self.inner.write().unwrap_or_else(PoisonError::into_inner);
        let mut next = Vec::with_capacity(credentials.len());
        for mut cred in credentials {
            let same = inner
                .creds
                .iter()
                .find(|c| c.id == cred.id && c.source == cred.source && c.metadata == cred.metadata);
            match same {
                Some(existing) => next.push(existing.clone()),
                None => {
                    inner.generation += 1;
                    cred.revision = inner.generation;
                    next.push(Arc::new(cred));
                }
            }
        }
        inner.creds = next;
    }

    fn read(&self) -> std::sync::RwLockReadGuard<'_, Inner> {
        self.inner.read().unwrap_or_else(PoisonError::into_inner)
    }
}

/// Writes `metadata` the way Go's `json.NewEncoder(f).Encode` does (compact JSON and a
/// newline) via an exclusively created, uniquely named 0600 sibling and a rename. The
/// temp name does not end in `.json`, so a crash never leaves a loadable credential.
fn write_atomic(path: &Path, metadata: &Map<String, Value>) -> std::io::Result<()> {
    use std::os::unix::fs::OpenOptionsExt;
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let mut bytes = serde_json::to_vec(metadata)?;
    bytes.push(b'\n');
    let dir = path.parent().unwrap_or(Path::new("."));
    let name = path.file_name().unwrap_or_default().to_string_lossy();
    let (tmp, mut file) = loop {
        let tmp = dir.join(format!(
            ".{name}.{}.{}.tmp",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        // create_new is O_CREAT|O_EXCL: it never follows or reuses an existing path.
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&tmp)
        {
            Ok(file) => break (tmp, file),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        }
    };
    let result = file
        .write_all(&bytes)
        .and_then(|()| file.sync_all())
        .and_then(|()| std::fs::rename(&tmp, path));
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

/// A response stream that reports its lease's outcome on end, on the first error (after
/// which it yields nothing more), or as `Cancelled` when dropped early.
pub struct Completing {
    inner: ExecStream,
    lease: Option<Lease>,
}

impl Completing {
    pub fn new(inner: ExecStream, lease: Lease) -> Self {
        Self {
            inner,
            lease: Some(lease),
        }
    }
}

impl Stream for Completing {
    type Item = Result<Bytes, ExecError>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        if this.lease.is_none() {
            return Poll::Ready(None);
        }
        let item = ready!(this.inner.poll_next_unpin(cx));
        match &item {
            Some(Ok(_)) => {}
            None => this.lease.take().unwrap().complete(Outcome::Success),
            Some(Err(e)) => this.lease.take().unwrap().complete(Outcome::Failure(e.clone())),
        }
        Poll::Ready(item)
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

    fn sel(provider: &str) -> Selection {
        Selection {
            provider: provider.into(),
            ..Selection::default()
        }
    }

    #[test]
    fn select_round_robins_enabled_credentials_and_honours_exclusions() {
        let store = CredentialStore::new(vec![
            cred("c1.json", "claude", false),
            cred("off.json", "claude", true),
            cred("x.json", "codex", false),
            cred("c2.json", "claude", false),
        ]);
        let picks: Vec<String> = (0..4)
            .map(|_| store.select(sel("claude")).unwrap().credential.id.clone())
            .collect();
        assert_eq!(picks, ["c1.json", "c2.json", "c1.json", "c2.json"]);
        assert!(store.select(sel("gemini")).is_none());
        let only = Selection {
            exclude: vec!["c1.json".into()],
            ..sel("claude")
        };
        for _ in 0..3 {
            assert_eq!(store.select(only.clone()).unwrap().credential.id, "c2.json");
        }
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
        // A stale temp-file-looking path and a symlink must not be reused or followed.
        let victim = dir.join("victim");
        std::fs::write(&victim, "keep").unwrap();
        std::os::unix::fs::symlink(
            &victim,
            dir.join(format!(".claude-a.json.{}.0.tmp", std::process::id())),
        )
        .unwrap();
        let store = CredentialStore::new(cpa_core::credential::load_dir(&dir).unwrap());
        let rev = store.get("claude-a.json").unwrap().revision;

        let mut patch = MetadataPatch::default();
        patch.set.insert("access_token".into(), "new".into());
        patch.set.insert("email".into(), "a@x.test".into());
        patch.set.insert("disabled".into(), true.into());
        let updated = store.apply_patch("claude-a.json", rev, &patch).unwrap();
        assert!(updated.revision > rev);
        assert!(updated.disabled);
        assert_eq!(updated.label, "a@x.test", "derived label follows the new email");
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "{\"type\":\"claude\",\"access_token\":\"new\",\"zz_unknown\":{\"k\":[1,2]},\"email\":\"a@x.test\",\"disabled\":true}\n"
        );
        assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        assert_eq!(
            std::fs::read_to_string(&victim).unwrap(),
            "keep",
            "symlink target untouched"
        );
        assert!(
            store.select(sel("claude")).is_none(),
            "patched-disabled credential leaves rotation"
        );

        assert_eq!(
            store.apply_patch("claude-a.json", rev, &patch).unwrap_err(),
            PatchError::Stale {
                current: updated.revision
            }
        );
        assert_eq!(
            store.apply_patch("nope.json", 0, &patch).unwrap_err(),
            PatchError::NotFound
        );
        let mut retype = MetadataPatch::default();
        retype.set.insert("type".into(), "codex".into());
        assert_eq!(
            store
                .apply_patch("claude-a.json", updated.revision, &retype)
                .unwrap_err(),
            PatchError::TypeIsImmutable
        );
        let leftovers: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .filter(|n| n.ends_with(".tmp") && !n.ends_with(".0.tmp"))
            .collect();
        assert!(leftovers.is_empty(), "no temp files left behind: {leftovers:?}");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn reconcile_keeps_unchanged_revisions_and_never_reuses_old_ones() {
        let store = CredentialStore::new(vec![cred("a.json", "claude", false), cred("b.json", "claude", false)]);
        let a = store.get("a.json").unwrap();
        let b = store.get("b.json").unwrap();
        store.reconcile(vec![cred("a.json", "claude", false)]);
        assert!(
            Arc::ptr_eq(&store.get("a.json").unwrap(), &a),
            "unchanged credential kept as is"
        );
        assert!(store.get("b.json").is_none());
        store.reconcile(vec![cred("a.json", "claude", false), cred("b.json", "claude", false)]);
        assert!(
            store.get("b.json").unwrap().revision > b.revision,
            "re-created credential gets a new revision"
        );
    }

    #[tokio::test]
    async fn every_lease_reports_exactly_once() {
        let store = CredentialStore::new(vec![cred("a.json", "claude", false)]);
        let stats = |s, f, c| AttemptStats {
            success: s,
            failure: f,
            cancelled: c,
        };

        // Dropped without completing (client gone mid-execute): cancelled.
        drop(store.select(sel("claude")).unwrap());
        assert_eq!(store.stats(), stats(0, 0, 1));
        // Completed explicitly: the drop that follows does not report again.
        store.select(sel("claude")).unwrap().complete(Outcome::Success);
        assert_eq!(store.stats(), stats(1, 0, 1));

        // A stream that errors and would then hang forever: the wrapper stops after Err.
        let err = ExecError::local(502, FailureScope::Credential, "boom");
        let inner = futures_util::stream::iter(vec![Ok(Bytes::from_static(b"a")), Err(err)])
            .chain(futures_util::stream::pending())
            .boxed();
        let mut s = Completing::new(inner, store.select(sel("claude")).unwrap());
        assert!(s.next().await.unwrap().is_ok());
        assert!(s.next().await.unwrap().is_err());
        assert!(s.next().await.is_none(), "fused after the first error");
        drop(s);
        assert_eq!(store.stats(), stats(1, 1, 1));

        let mut done = Completing::new(
            futures_util::stream::empty().boxed(),
            store.select(sel("claude")).unwrap(),
        );
        assert!(done.next().await.is_none());
        drop(done);
        assert_eq!(store.stats(), stats(2, 1, 1));

        // Dropped mid-stream: cancelled.
        let mut mid = Completing::new(
            futures_util::stream::iter(vec![Ok(Bytes::new())])
                .chain(futures_util::stream::pending())
                .boxed(),
            store.select(sel("claude")).unwrap(),
        );
        assert!(mid.next().await.is_some());
        drop(mid);
        assert_eq!(store.stats(), stats(2, 1, 2));
    }
}
