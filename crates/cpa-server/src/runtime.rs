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
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError, RwLock};
use std::task::{Context, Poll, ready};
use std::time::{Duration, Instant};

use bytes::Bytes;
use cpa_core::config::Config;
use cpa_core::credential::{Credential, MetadataPatch, Source};
use cpa_core::exec::{ExecError, ExecStream, FailureScope};
use cpa_exec::Executors;
use futures_util::{Stream, StreamExt};
use serde_json::{Map, Value};

use crate::refresh::RefreshState;
use crate::scheduler::{Policy, Scheduler, execution_model, retry_status};

pub struct Runtime {
    config: RwLock<Arc<Config>>,
    store: Arc<CredentialStore>,
    pub executors: Executors,
    refresh_task: Mutex<Option<tokio::task::AbortHandle>>,
    refresh_state: Mutex<RefreshState>,
}

impl Runtime {
    /// The scheduler policy is derived from `config.routing`.
    pub fn new(config: Config, credentials: Vec<Credential>, executors: Executors) -> Self {
        let policy = Policy::from(&config.routing);
        let rt = Self {
            config: RwLock::new(Arc::new(config)),
            store: CredentialStore::new(credentials),
            executors,
            refresh_task: Mutex::default(),
            refresh_state: Mutex::default(),
        };
        rt.publish_policy(policy);
        rt
    }

    /// The config snapshot to use for one whole request.
    pub fn config(&self) -> Arc<Config> {
        self.config.read().unwrap_or_else(PoisonError::into_inner).clone()
    }

    /// Replaces the config and the scheduler policy derived from it. Requests already
    /// running keep their snapshot.
    pub fn publish_config(&self, config: Config) {
        let policy = Policy::from(&config.routing);
        self.publish_config_and_policy(config, policy);
    }

    /// Integration should use this when publishing parsed routing settings too.
    pub fn publish_config_and_policy(&self, config: Config, policy: Policy) {
        let mut current = self.config.write().unwrap_or_else(PoisonError::into_inner);
        *current = Arc::new(config);
        self.publish_policy(policy);
    }

    /// A coherent config/policy pair for the entire route attempt loop.
    pub fn request_snapshot(&self) -> (Arc<Config>, Arc<Policy>) {
        let config = self.config.read().unwrap_or_else(PoisonError::into_inner);
        (config.clone(), self.policy())
    }

    pub fn store(&self) -> &Arc<CredentialStore> {
        &self.store
    }

    pub fn policy(&self) -> Arc<Policy> {
        self.store.policy.read().unwrap_or_else(PoisonError::into_inner).clone()
    }

    pub fn publish_policy(&self, policy: Policy) {
        let mut current = self.store.policy.write().unwrap_or_else(PoisonError::into_inner);
        self.store
            .scheduler
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .configure(&current, &policy);
        *current = Arc::new(policy);
    }

    /// Selects a credential and prepares it if its executor asks to, single-flighted per
    /// credential. The returned lease holds the committed, prepared snapshot.
    pub async fn acquire(&self, selection: Selection, cfg: &Config) -> Result<Lease, AcquireError> {
        self.acquire_with_policy(selection, cfg, self.policy()).await
    }

    pub async fn acquire_with_policy(
        &self,
        selection: Selection,
        cfg: &Config,
        policy: Arc<Policy>,
    ) -> Result<Lease, AcquireError> {
        let mut lease = self.store.select_with_policy(selection, policy)?;
        if !self.executors.needs_prepare(&lease.credential, cfg) {
            return Ok(lease);
        }
        let id = lease.credential.id.clone();
        match self.prepare_credential(&id, cfg).await {
            Ok(current) => lease.credential = current,
            Err(error) => {
                lease.complete(Outcome::Failure(error.clone()));
                return Err(AcquireError::Prepare { id, error });
            }
        }
        lease.execution_model = execution_model(&lease.credential, &lease.selection.model, &lease.policy)
            .ok_or(AcquireError::NoCredential)?;
        Ok(lease)
    }

    async fn prepare_credential(&self, id: &str, cfg: &Config) -> Result<Arc<Credential>, ExecError> {
        let lock = self.store.prepare_lock(id);
        let _guard = lock.lock().await;
        // Another request may have prepared it while we waited.
        let current = self.store.get(id).filter(|c| !c.disabled).ok_or_else(|| {
            ExecError::local(
                409,
                FailureScope::Request,
                "credential removed or disabled during preparation",
            )
        })?;
        if !self.executors.needs_prepare(&current, cfg) {
            return Ok(current);
        }
        let patch = self.executors.prepare(&current, cfg).await?;
        let store = self.store.clone();
        let revision = current.revision;
        let id = id.to_owned();
        tokio::task::spawn_blocking(move || store.apply_patch(&id, revision, &patch))
            .await
            .map_err(|e| PatchError::Io(e.to_string()))
            .and_then(|r| r)
            .map_err(|e| {
                ExecError::local(
                    500,
                    FailureScope::Request,
                    format!("committing prepared credential: {e:?}"),
                )
            })
    }

    /// One replaceable refresh loop. Uses the same preparation lock and atomic
    /// revision-checked commit as request acquisition; executors never persist files.
    /// ponytail: needs_prepare controls eligibility. Unauthorized lifecycle gating
    /// and end-to-end token rotation await executor/lifecycle integration (M4-0027).
    pub fn start_auto_refresh(self: &Arc<Self>) {
        let mut task = self.refresh_task.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(previous) = task.take() {
            previous.abort();
        }
        let weak = Arc::downgrade(self);
        let handle = tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(5));
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                interval.tick().await;
                let Some(rt) = weak.upgrade() else {
                    break;
                };
                let cfg = rt.config();
                let snapshot = rt.store.snapshot();
                let jobs: Vec<_> = {
                    let mut state = rt.refresh_state.lock().unwrap_or_else(PoisonError::into_inner);
                    state.reconcile(&snapshot);
                    snapshot
                        .iter()
                        .filter(|c| matches!(c.source, Source::File(_)) && !c.disabled)
                        .filter(|c| rt.executors.needs_prepare(c, &cfg))
                        .filter(|c| state.reserve(c, Instant::now()))
                        .cloned()
                        .collect()
                };
                // ponytail: bounded batches, so a slow worker delays the next scan.
                // Use an independent queue if refresh latency matters for large pools.
                futures_util::stream::iter(jobs)
                    .for_each_concurrent(16, |credential| {
                        let rt = rt.clone();
                        let cfg = cfg.clone();
                        async move {
                            let result = rt.prepare_credential(&credential.id, &cfg).await;
                            let Some(current) = rt.store.get(&credential.id) else {
                                return;
                            };
                            // A concurrent removal/re-import or management edit must not
                            // transfer the old refresh failure/backoff to the replacement.
                            if result.is_err() && current.revision != credential.revision {
                                return;
                            }
                            if result
                                .as_ref()
                                .is_ok_and(|prepared| prepared.revision != current.revision)
                            {
                                return;
                            }
                            let ineffective = result.is_ok() && rt.executors.needs_prepare(&current, &cfg);
                            rt.refresh_state.lock().unwrap_or_else(PoisonError::into_inner).finish(
                                &current,
                                result.as_ref().err(),
                                ineffective,
                                Instant::now(),
                            );
                        }
                    })
                    .await;
            }
        });
        *task = Some(handle.abort_handle());
    }

    pub fn stop_auto_refresh(&self) {
        if let Some(task) = self.refresh_task.lock().unwrap_or_else(PoisonError::into_inner).take() {
            task.abort();
        }
    }
}

impl Drop for Runtime {
    fn drop(&mut self) {
        if let Some(task) = self
            .refresh_task
            .get_mut()
            .unwrap_or_else(PoisonError::into_inner)
            .take()
        {
            task.abort();
        }
    }
}

#[derive(Debug)]
pub enum AcquireError {
    NoCredential,
    Cooldown { wait: Duration },
    Prepare { id: String, error: ExecError },
}

/// Route-model state is separate from the lease's resolved execution model.
#[derive(Debug, Clone, Default)]
pub struct Selection {
    pub provider: String,
    pub model: String,
    pub session: Option<String>,
    /// Credential IDs already tried in this request.
    pub exclude: Vec<String>,
    pub retry_round: usize,
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
    pub execution_model: String,
    pub attempt: u64,
    policy: Arc<Policy>,
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
    policy: RwLock<Arc<Policy>>,
    scheduler: Mutex<Scheduler>,
    attempts: AtomicU64,
    stats: [AtomicU64; 3],
    prepare_locks: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
    activity: Mutex<HashMap<String, CredentialActivity>>,
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
            policy: RwLock::new(Arc::new(Policy::default())),
            scheduler: Mutex::default(),
            attempts: AtomicU64::new(0),
            stats: Default::default(),
            prepare_locks: Mutex::default(),
            activity: Mutex::default(),
        })
    }

    pub fn snapshot(&self) -> Vec<Arc<Credential>> {
        self.read().creds.clone()
    }

    pub fn get(&self, id: &str) -> Option<Arc<Credential>> {
        self.read().creds.iter().find(|c| c.id == id).cloned()
    }

    pub fn select(self: &Arc<Self>, selection: Selection) -> Option<Lease> {
        let policy = self.policy.read().unwrap_or_else(PoisonError::into_inner).clone();
        self.select_with_policy(selection, policy).ok()
    }

    pub fn select_with_policy(
        self: &Arc<Self>,
        selection: Selection,
        policy: Arc<Policy>,
    ) -> Result<Lease, AcquireError> {
        let now = Instant::now();
        let credential = {
            let inner = self.read();
            let mut scheduler = self.scheduler.lock().unwrap_or_else(PoisonError::into_inner);
            let eligible: Vec<_> = inner
                .creds
                .iter()
                .filter(|c| c.provider == selection.provider && !c.disabled && !selection.exclude.contains(&c.id))
                .filter(|c| policy.retry_limit(c) >= selection.retry_round && scheduler.admits(c, &policy))
                .filter_map(|c| execution_model(c, &selection.model, &policy).map(|m| (c, m)))
                .collect();
            let candidates: Vec<_> = eligible
                .iter()
                .filter(|(c, m)| scheduler.wait(c, m, now).is_none())
                .map(|(c, _)| c.as_ref())
                .collect();
            if candidates.is_empty() {
                if !eligible.is_empty()
                    && eligible.iter().all(|(c, m)| scheduler.quota_cooling(c, m, now))
                    && let Some(wait) = eligible.iter().filter_map(|(c, m)| scheduler.wait(c, m, now)).min()
                {
                    return Err(AcquireError::Cooldown { wait });
                }
                return Err(AcquireError::NoCredential);
            }
            let picked = scheduler.pick(&candidates, &selection, &policy, now).unwrap();
            inner.creds.iter().find(|c| c.id == picked.id).unwrap().clone()
        };
        let execution_model = execution_model(&credential, &selection.model, &policy).unwrap();
        Ok(Lease {
            store: self.clone(),
            credential,
            selection,
            execution_model,
            attempt: self.attempts.fetch_add(1, Ordering::Relaxed),
            policy,
            reported: false,
        })
    }

    /// Returns the next round's wait, if any credential still permits that round.
    /// A wait exceeding the cap is rejected, not shortened (Go conductor_selection.go).
    pub fn retry_wait(&self, selection: &Selection, policy: &Policy, error: &ExecError) -> Option<Duration> {
        self.retry_wait_at(selection, policy, error, Instant::now())
    }

    fn retry_wait_at(
        &self,
        selection: &Selection,
        policy: &Policy,
        error: &ExecError,
        now: Instant,
    ) -> Option<Duration> {
        if error.scope == FailureScope::Request
            || (!retry_status(error.status) && error.scope != FailureScope::Transport)
        {
            return None;
        }
        let inner = self.read();
        let scheduler = self.scheduler.lock().unwrap_or_else(PoisonError::into_inner);
        let wait = inner
            .creds
            .iter()
            .filter(|c| c.provider == selection.provider && !c.disabled)
            .filter(|c| policy.retry_limit(c) > selection.retry_round && scheduler.admits(c, policy))
            .filter_map(|c| execution_model(c, &selection.model, policy).map(|m| (c, m)))
            .filter(|(c, m)| scheduler.retry_eligible(c, m, now))
            .map(|(c, m)| {
                let wait = scheduler.wait(c, &m, now).unwrap_or_default();
                if error.status == 429 && selection.exclude.contains(&c.id) && !policy.cooling_disabled(c) {
                    wait.max(Duration::from_secs(10))
                } else {
                    wait
                }
            })
            .min()?;
        if wait > policy.max_retry_interval {
            None
        } else {
            Some(wait)
        }
    }

    fn record(&self, lease: &Lease, outcome: &Outcome) {
        let slot = match outcome {
            Outcome::Success => 0,
            Outcome::Failure(_) => 1,
            Outcome::Cancelled => 2,
        };
        self.stats[slot].fetch_add(1, Ordering::Relaxed);
        if !matches!(outcome, Outcome::Cancelled) {
            self.note_activity(&lease.credential.id, matches!(outcome, Outcome::Success));
        }
        let inner = self.read();
        // Outcomes from credentials deleted/re-created during an attempt must not
        // poison the replacement. Metadata edits likewise invalidate stale results.
        if inner
            .creds
            .iter()
            .any(|c| c.id == lease.credential.id && c.revision == lease.credential.revision)
        {
            self.scheduler.lock().unwrap_or_else(PoisonError::into_inner).record(
                &lease.credential,
                &lease.execution_model,
                outcome,
                &lease.policy,
                Instant::now(),
            );
        }
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
            let same = inner.creds.iter().find(|c| {
                c.id == cred.id
                    && c.source == cred.source
                    && c.metadata == cred.metadata
                    && c.attributes == cred.attributes
                    && c.provider == cred.provider
                    && c.disabled == cred.disabled
                    && c.label == cred.label
            });
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
        self.scheduler
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .reconcile(&inner.creds);
    }

    fn read(&self) -> std::sync::RwLockReadGuard<'_, Inner> {
        self.inner.read().unwrap_or_else(PoisonError::into_inner)
    }
}

/// Per-credential outcome counters for management views (Go `Auth.Success`/`Failed`
/// and its 20 x 10-minute recent-request ring). Additive read API.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CredentialActivity {
    pub success: u64,
    pub failed: u64,
    /// `(bucket, success, failed)` for the most recent buckets, where `bucket` is
    /// Unix seconds / 600; at most 20 entries, oldest first.
    pub recent: Vec<(i64, u64, u64)>,
}

pub const RECENT_BUCKET_SECONDS: i64 = 600;
const RECENT_BUCKETS: usize = 20;

impl CredentialStore {
    fn note_activity(&self, id: &str, success: bool) {
        let bucket = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs() as i64 / RECENT_BUCKET_SECONDS);
        let mut all = self.activity.lock().unwrap_or_else(PoisonError::into_inner);
        let entry = all.entry(id.to_owned()).or_default();
        if success {
            entry.success += 1;
        } else {
            entry.failed += 1;
        }
        match entry.recent.last_mut() {
            Some(last) if last.0 == bucket => {
                if success {
                    last.1 += 1;
                } else {
                    last.2 += 1;
                }
            }
            _ => entry.recent.push((bucket, u64::from(success), u64::from(!success))),
        }
        entry.recent.retain(|(b, _, _)| bucket - b < RECENT_BUCKETS as i64);
    }

    /// Counters for one credential; zero when it has served nothing yet.
    pub fn activity(&self, id: &str) -> CredentialActivity {
        self.activity
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(id)
            .cloned()
            .unwrap_or_default()
    }

    /// Active cooldowns of one credential.
    pub fn cooldowns(&self, id: &str) -> Vec<crate::scheduler::CooldownState> {
        self.scheduler
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .cooldowns_of(id, Instant::now())
    }

    /// Clears the cooldowns of one credential (Go `Manager.ResetQuota`); returns the
    /// model keys that were cooling.
    pub fn reset_cooldowns(&self, id: &str) -> Vec<String> {
        self.scheduler
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .reset_cooldowns(id)
    }
}

impl Runtime {
    /// Runs the executor's single-flighted preparation now if it is due (management
    /// refresh). Returns the committed credential.
    /// ponytail: refresh-if-due only; Go's forced refresh needs a force flag in the
    /// executor preparation contract.
    pub async fn refresh_credential(&self, id: &str) -> Result<Arc<Credential>, ExecError> {
        let cfg = self.config();
        self.prepare_credential(id, &cfg).await
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

    #[test]
    fn attribute_only_reload_changes_priority_and_rejects_stale_outcome() {
        let a = cred("a.json", "claude", false);
        let mut b = cred("b.json", "claude", false);
        b.attributes.insert("priority".into(), "1".into());
        let store = CredentialStore::new(vec![a.clone(), b.clone()]);
        let old = store.select(sel("claude")).unwrap();
        assert_eq!(old.credential.id, "b.json");
        let revision = old.credential.revision;
        b.attributes.insert("priority".into(), "2".into());
        store.reconcile(vec![a, b]);
        assert!(store.get("b.json").unwrap().revision > revision);
        old.complete(Outcome::Failure(ExecError::local(
            401,
            FailureScope::Credential,
            "expired",
        )));
        let current = store.select(sel("claude")).unwrap();
        assert_eq!(
            current.credential.id, "b.json",
            "stale outcome must not cool new revision"
        );
        assert_eq!(current.credential.attributes["priority"], "2");
        current.complete(Outcome::Success);
        assert_eq!(store.stats().failure, 1);
        assert_eq!(store.stats().success, 1);
    }

    #[test]
    fn stale_lease_cannot_poison_deleted_and_recreated_credential() {
        let credential = cred("a.json", "claude", false);
        let store = CredentialStore::new(vec![credential.clone()]);
        let old = store.select(sel("claude")).unwrap();
        store.reconcile(Vec::new());
        store.reconcile(vec![credential]);
        old.complete(Outcome::Failure(ExecError::local(
            429,
            FailureScope::Credential,
            "quota",
        )));
        store.select(sel("claude")).unwrap().complete(Outcome::Success);
        assert_eq!(store.stats().failure, 1);
        assert_eq!(store.stats().success, 1);
    }

    #[test]
    fn retry_wait_obeys_exact_cap_request_scope_and_quota_floor() {
        let store = CredentialStore::new(vec![cred("a.json", "claude", false)]);
        let now = Instant::now();
        let mut selection = sel("claude");
        selection.exclude.push("a.json".into());
        let mut policy = Policy {
            request_retry: 1,
            ..Policy::default()
        };
        let transport = ExecError::local(502, FailureScope::Transport, "connection lost");
        assert_eq!(
            store.retry_wait_at(&selection, &policy, &transport, now),
            Some(Duration::ZERO)
        );
        let request = ExecError::local(503, FailureScope::Request, "request invalid");
        assert_eq!(store.retry_wait_at(&selection, &policy, &request, now), None);
        let quota = ExecError::local(429, FailureScope::Model, "quota");
        assert_eq!(store.retry_wait_at(&selection, &policy, &quota, now), None);
        policy.max_retry_interval = Duration::from_secs(10);
        assert_eq!(
            store.retry_wait_at(&selection, &policy, &quota, now),
            Some(Duration::from_secs(10))
        );
        let credential = store.get("a.json").unwrap();
        store.scheduler.lock().unwrap().record(
            &credential,
            &selection.model,
            &Outcome::Failure(quota.clone()),
            &policy,
            now,
        );
        let later = now + Duration::from_millis(1);
        assert_eq!(
            store.retry_wait_at(&selection, &policy, &quota, later),
            Some(Duration::from_secs(10))
        );
        policy.max_retry_interval = Duration::from_secs(9);
        assert_eq!(store.retry_wait_at(&selection, &policy, &quota, later), None);
        policy.disable_cooling = true;
        store.scheduler.lock().unwrap().record(
            &credential,
            &selection.model,
            &Outcome::Failure(quota.clone()),
            &policy,
            later,
        );
        policy.max_retry_interval = Duration::ZERO;
        assert_eq!(
            store.retry_wait_at(&selection, &policy, &quota, later),
            Some(Duration::ZERO)
        );
        selection.retry_round = 1;
        assert_eq!(store.retry_wait_at(&selection, &policy, &transport, later), None);
    }

    #[test]
    fn config_routing_drives_policy_at_startup_and_on_publish() {
        let executors = || Executors {
            claude: cpa_exec::claude::ClaudeExecutor::new("http://127.0.0.1:1").unwrap(),
        };
        // Legacy top-level keys and the canonical routing block both reach the scheduler.
        let rt = Runtime::new(
            Config::parse("request-retry: 2\nrouting:\n  strategy: ff\n  session-affinity-ttl: 250ms\n").unwrap(),
            Vec::new(),
            executors(),
        );
        let policy = rt.policy();
        assert_eq!(policy.strategy, crate::scheduler::Strategy::FillFirst);
        assert_eq!(policy.request_retry, 2);
        assert_eq!(policy.session_affinity_ttl, Duration::from_secs(1));

        rt.publish_config(Config::parse("routing:\n  strategy: wrr\n").unwrap());
        let policy = rt.policy();
        assert_eq!(policy.strategy, crate::scheduler::Strategy::WeightedRoundRobin);
        assert_eq!(policy.request_retry, 0);
        assert_eq!(policy.session_affinity_ttl, Duration::from_secs(3600));
    }

    #[tokio::test]
    async fn preparation_waiter_observes_deletion_and_refresh_loop_is_replaceable() {
        let rt = Arc::new(Runtime::new(
            Config::parse("").unwrap(),
            vec![cred("a.json", "claude", false)],
            Executors {
                claude: cpa_exec::claude::ClaudeExecutor::new("http://127.0.0.1:1").unwrap(),
            },
        ));
        let lock = rt.store.prepare_lock("a.json");
        let guard = lock.lock().await;
        let worker = {
            let rt = rt.clone();
            tokio::spawn(async move { rt.prepare_credential("a.json", &rt.config()).await })
        };
        tokio::task::yield_now().await;
        assert!(!worker.is_finished());
        rt.store.reconcile(Vec::new());
        drop(guard);
        let error = worker.await.unwrap().unwrap_err();
        assert_eq!(error.scope, FailureScope::Request);
        assert!(rt.store.snapshot().is_empty());
        rt.start_auto_refresh();
        let first = rt.refresh_task.lock().unwrap().clone().unwrap();
        rt.start_auto_refresh();
        tokio::task::yield_now().await;
        assert!(first.is_finished());
        let second = rt.refresh_task.lock().unwrap().clone().unwrap();
        rt.stop_auto_refresh();
        tokio::task::yield_now().await;
        assert!(second.is_finished());
        assert!(rt.refresh_task.lock().unwrap().is_none());
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
        let err = ExecError::local(502, FailureScope::Transport, "boom");
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
