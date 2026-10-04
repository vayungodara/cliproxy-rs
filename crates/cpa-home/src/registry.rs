//! Executions dispatched by Home during one subscriber lifetime (Go
//! sdk/cliproxy/executionregistry).
//!
//! A dispatch first reserves a [`Pending`] token, so a lifetime can wait for every
//! request whose Home reply is still unknown. Installing the reply turns the token into
//! a [`Scope`] that owns the execution's resources. Ending an accounted scope bumps the
//! cumulative release sequence of its (credential, model) group and hands it to the
//! release sink, which reports it to Home. `drain` refuses new work, cancels bound
//! resources and waits for every owner to end.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock, PoisonError};
use std::time::{Duration, SystemTime};

use tokio::sync::watch;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RegistryError {
    NotAccepting,
    Closed,
    InvalidResource,
    AlreadyBound,
    Timeout,
}

impl std::fmt::Display for RegistryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            RegistryError::NotAccepting => "execution registry is not accepting dispatches",
            RegistryError::Closed => "execution registry is closed",
            RegistryError::InvalidResource => "invalid execution resource",
            RegistryError::AlreadyBound => "execution resource is already bound",
            RegistryError::Timeout => "context deadline exceeded",
        })
    }
}

impl std::error::Error for RegistryError {}

const ACCEPTING: u8 = 0;
const DRAINING: u8 = 1;
const CLOSED: u8 = 2;

/// Go `ScopeSpec`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScopeSpec {
    pub request_id: String,
    pub credential_id: String,
    pub model: String,
    /// `http`, `stream` or `websocket`.
    pub kind: String,
    pub started_at: SystemTime,
    pub accounted: bool,
}

/// The cumulative release sequence key: one accounted credential and model.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ReleaseGroup {
    pub credential_id: String,
    pub model: String,
}

/// Completes once Home acknowledged `sequence` for `group`.
#[derive(Debug, Clone)]
pub struct ReleaseTicket {
    pub group: ReleaseGroup,
    pub sequence: i64,
    acked: watch::Receiver<i64>,
}

impl ReleaseTicket {
    pub fn new(group: ReleaseGroup, sequence: i64, acked: watch::Receiver<i64>) -> Self {
        Self { group, sequence, acked }
    }

    /// Waits for the acknowledgement, at most `bound`.
    pub async fn wait(&self, bound: Duration) -> Result<(), RegistryError> {
        let mut acked = self.acked.clone();
        let sequence = self.sequence;
        match tokio::time::timeout(bound, acked.wait_for(|a| *a >= sequence)).await {
            Ok(Ok(_)) => Ok(()),
            Ok(Err(_)) => Err(RegistryError::Closed),
            Err(_) => Err(RegistryError::Timeout),
        }
    }
}

/// Receives the latest cumulative sequence of a group; may return an ack ticket.
pub type ReleaseSink = Arc<dyn Fn(ReleaseGroup, i64) -> Option<ReleaseTicket> + Send + Sync>;

/// Go `Observation`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Observation {
    pub request_id: String,
    pub credential_id: String,
    pub model: String,
    pub request_kind: String,
    pub started_at: SystemTime,
    pub accounted: bool,
}

/// Go `Freeze`: an immutable in-flight snapshot.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Freeze {
    pub revision: i64,
    pub barrier_revision: i64,
    pub executions: Vec<Observation>,
}

type CloseFn = Box<dyn FnOnce() + Send>;

#[derive(Default)]
struct Tables {
    next: u64,
    snapshot_revision: i64,
    observed_barrier: i64,
    pending_barrier_sequence: u64,
    published_barrier: i64,
    pending: HashSet<u64>,
    scopes: HashMap<u64, Arc<ScopeInner>>,
    release_sequences: HashMap<ReleaseGroup, i64>,
    release_sink: Option<ReleaseSink>,
}

struct Shared {
    state: AtomicU8,
    tables: Mutex<Tables>,
    changed: watch::Sender<u64>,
}

/// One subscriber lifetime's executions. Cloning shares the registry.
#[derive(Clone)]
pub struct Registry(Arc<Shared>);

impl Default for Registry {
    fn default() -> Self {
        Self::new()
    }
}

impl Registry {
    pub fn new() -> Self {
        Self(Arc::new(Shared {
            state: AtomicU8::new(ACCEPTING),
            tables: Mutex::default(),
            changed: watch::channel(0).0,
        }))
    }

    pub fn ptr_eq(&self, other: &Registry) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }

    fn tables(&self) -> std::sync::MutexGuard<'_, Tables> {
        self.0.tables.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn signal(&self) {
        self.0.changed.send_modify(|n| *n = n.wrapping_add(1));
    }

    fn accepting(&self) -> bool {
        self.0.state.load(Ordering::SeqCst) == ACCEPTING
    }

    /// Go `BeginDispatch`.
    pub fn begin_dispatch(&self) -> Result<Pending, RegistryError> {
        if !self.accepting() {
            return Err(RegistryError::NotAccepting);
        }
        let mut tables = self.tables();
        if !self.accepting() {
            return Err(RegistryError::NotAccepting);
        }
        tables.next += 1;
        let id = tables.next;
        tables.pending.insert(id);
        Ok(Pending {
            id,
            registry: Some(self.clone()),
        })
    }

    /// Go `Install`: turns a pending token into a live scope.
    pub fn install(&self, mut pending: Pending, spec: ScopeSpec) -> Result<Scope, RegistryError> {
        let Some(owner) = pending.registry.take() else {
            return Err(RegistryError::InvalidResource);
        };
        if !owner.ptr_eq(self) {
            owner.end_pending(pending.id);
            return Err(RegistryError::InvalidResource);
        }
        let mut tables = self.tables();
        if !tables.pending.remove(&pending.id) {
            return Err(RegistryError::InvalidResource);
        }
        if !self.accepting() {
            drop(tables);
            self.signal();
            return Err(RegistryError::NotAccepting);
        }
        let inner = Arc::new(ScopeInner {
            id: pending.id,
            registry: self.clone(),
            spec,
            resource: Mutex::new(Resource::Unbound),
            ended: OnceLock::new(),
        });
        tables.scopes.insert(pending.id, inner.clone());
        drop(tables);
        self.signal();
        Ok(Scope(Arc::new(Owner(inner))))
    }

    fn end_pending(&self, id: u64) {
        if self.tables().pending.remove(&id) {
            self.signal();
        }
    }

    /// Go `WaitPending`: every unresolved dispatch has ended or installed.
    pub async fn wait_pending(&self, bound: Duration) -> Result<(), RegistryError> {
        self.wait_until(bound, |t| t.pending.is_empty()).await
    }

    async fn wait_until(&self, bound: Duration, done: impl Fn(&Tables) -> bool) -> Result<(), RegistryError> {
        let mut changed = self.0.changed.subscribe();
        let wait = async {
            loop {
                if done(&self.tables()) {
                    return;
                }
                if changed.changed().await.is_err() {
                    return;
                }
            }
        };
        tokio::time::timeout(bound, wait)
            .await
            .map_err(|_| RegistryError::Timeout)
    }

    /// Go `SetReleaseSink`: replays every known group to the new sink.
    pub fn set_release_sink(&self, sink: Option<ReleaseSink>) {
        let sequences: Vec<(ReleaseGroup, i64)> = {
            let mut tables = self.tables();
            tables.release_sink.clone_from(&sink);
            tables.release_sequences.iter().map(|(g, s)| (g.clone(), *s)).collect()
        };
        if let Some(sink) = sink {
            for (group, sequence) in sequences {
                if sequence > 0 {
                    sink(group, sequence);
                }
            }
        }
    }

    /// Go `Drain`: refuse new work, cancel bound resources, wait for every owner.
    pub async fn drain(&self, bound: Duration) -> Result<(), RegistryError> {
        if self
            .0
            .state
            .compare_exchange(ACCEPTING, DRAINING, Ordering::SeqCst, Ordering::SeqCst)
            .is_err()
            && self.0.state.load(Ordering::SeqCst) != DRAINING
        {
            return Err(RegistryError::Closed);
        }
        let scopes: Vec<Arc<ScopeInner>> = self.tables().scopes.values().cloned().collect();
        // Closes run off the async workers, so a slow one cannot defeat `bound`; the
        // owners end their scopes once the resources are gone.
        for scope in scopes {
            if let Some((close, done)) = scope.begin_close() {
                tokio::task::spawn_blocking(move || {
                    close();
                    done.finish();
                });
            }
        }
        self.wait_until(bound, |t| t.pending.is_empty() && t.scopes.is_empty())
            .await?;
        self.0.state.store(CLOSED, Ordering::SeqCst);
        Ok(())
    }

    /// Go `Close`: permanently rejects work and closes every bound resource.
    pub fn close(&self) {
        self.0.state.store(CLOSED, Ordering::SeqCst);
        let scopes: Vec<Arc<ScopeInner>> = self.tables().scopes.values().cloned().collect();
        for scope in scopes {
            scope.close_now();
        }
    }

    /// Go `ObserveBarrier`: Home's latest observation barrier.
    pub fn observe_barrier(&self, revision: i64) {
        if revision <= 0 {
            return;
        }
        let mut tables = self.tables();
        if revision > tables.observed_barrier {
            tables.observed_barrier = revision;
            tables.pending_barrier_sequence = tables.next;
        }
    }

    /// Go `FreezeInFlight`. The barrier is published only once every dispatch that
    /// began before Home raised it has resolved.
    pub fn freeze(&self) -> Freeze {
        let mut tables = self.tables();
        if tables.observed_barrier > tables.published_barrier {
            let threshold = tables.pending_barrier_sequence;
            if !tables.pending.iter().any(|id| *id <= threshold) {
                tables.published_barrier = tables.observed_barrier;
            }
        }
        tables.snapshot_revision += 1;
        Freeze {
            revision: tables.snapshot_revision,
            barrier_revision: tables.published_barrier,
            executions: tables
                .scopes
                .values()
                .map(|scope| Observation {
                    request_id: scope.spec.request_id.clone(),
                    credential_id: scope.spec.credential_id.clone(),
                    model: scope.spec.model.clone(),
                    request_kind: scope.spec.kind.clone(),
                    started_at: scope.spec.started_at,
                    accounted: scope.spec.accounted,
                })
                .collect(),
        }
    }

    pub fn active(&self) -> usize {
        self.tables().scopes.len()
    }

    pub fn pending(&self) -> usize {
        self.tables().pending.len()
    }
}

/// A reserved dispatch slot. Dropping it without installing ends it (Go `End`).
pub struct Pending {
    id: u64,
    registry: Option<Registry>,
}

impl Drop for Pending {
    fn drop(&mut self) {
        if let Some(registry) = self.registry.take() {
            registry.end_pending(self.id);
        }
    }
}

/// Set once a started resource close has finished.
#[derive(Default)]
struct Completion {
    done: Mutex<bool>,
    finished: Condvar,
}

impl Completion {
    fn finish(&self) {
        *self.done.lock().unwrap_or_else(PoisonError::into_inner) = true;
        self.finished.notify_all();
    }

    fn wait(&self) {
        let mut done = self.done.lock().unwrap_or_else(PoisonError::into_inner);
        while !*done {
            done = self.finished.wait(done).unwrap_or_else(PoisonError::into_inner);
        }
    }
}

enum Resource {
    Unbound,
    Bound(CloseFn),
    /// A close started; waiters join its completion (Go `closeDone`).
    Closing(Arc<Completion>),
    Closed,
}

struct ScopeInner {
    id: u64,
    registry: Registry,
    spec: ScopeSpec,
    resource: Mutex<Resource>,
    ended: OnceLock<Option<ReleaseTicket>>,
}

impl ScopeInner {
    fn resource(&self) -> std::sync::MutexGuard<'_, Resource> {
        self.resource.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Go `startBoundResourceClose`: the close to run and its completion, unless
    /// nothing is bound or a close already started.
    fn begin_close(&self) -> Option<(CloseFn, Arc<Completion>)> {
        let mut resource = self.resource();
        match std::mem::replace(&mut *resource, Resource::Closed) {
            Resource::Bound(close) => {
                let done = Arc::new(Completion::default());
                *resource = Resource::Closing(done.clone());
                Some((close, done))
            }
            Resource::Closing(done) => {
                *resource = Resource::Closing(done);
                None
            }
            Resource::Unbound | Resource::Closed => None,
        }
    }

    /// Go `waitForBoundResourceClose`: closes here, or joins a close in progress.
    fn close_now(&self) {
        if let Some((close, done)) = self.begin_close() {
            close();
            done.finish();
        }
        let running = match &*self.resource() {
            Resource::Closing(done) => Some(done.clone()),
            _ => None,
        };
        if let Some(done) = running {
            done.wait();
        }
    }

    /// Go `EndWithRelease`. The scope stays registered until the release sink took
    /// its sequence, so a drain that sees an empty registry also sees the release.
    fn end(&self) -> Option<ReleaseTicket> {
        self.ended
            .get_or_init(|| {
                self.close_now();
                let registry = &self.registry;
                let (sink, group, sequence) = {
                    let mut tables = registry.tables();
                    if self.spec.accounted {
                        let group = ReleaseGroup {
                            credential_id: self.spec.credential_id.clone(),
                            model: self.spec.model.clone(),
                        };
                        let sequence = tables.release_sequences.entry(group.clone()).or_default();
                        *sequence += 1;
                        let sequence = *sequence;
                        (tables.release_sink.clone(), Some(group), sequence)
                    } else {
                        (None, None, 0)
                    }
                };
                let ticket = match (sink, group) {
                    (Some(sink), Some(group)) if sequence > 0 => sink(group, sequence),
                    _ => None,
                };
                registry.tables().scopes.remove(&self.id);
                registry.signal();
                ticket
            })
            .clone()
    }
}

/// Ends the scope when the last handle goes away without an explicit end.
struct Owner(Arc<ScopeInner>);

impl Drop for Owner {
    fn drop(&mut self) {
        self.0.end();
    }
}

/// An installed execution. Clones share it; it ends once, at the latest when the last
/// clone is dropped (the registry would otherwise wait for it until drain times out).
#[derive(Clone)]
pub struct Scope(Arc<Owner>);

impl Scope {
    fn inner(&self) -> &ScopeInner {
        &self.0.0
    }

    pub fn spec(&self) -> &ScopeSpec {
        &self.inner().spec
    }

    pub fn active(&self) -> bool {
        self.inner().ended.get().is_none()
    }

    /// Go `Bind`: the scope's one resource, closed when it ends or drains. It should
    /// cancel and return promptly: ends wait for it.
    pub fn bind(&self, close: impl FnOnce() + Send + 'static) -> Result<(), RegistryError> {
        let registry = &self.inner().registry;
        let _tables = registry.tables();
        if !registry.accepting() || !self.active() {
            return Err(RegistryError::NotAccepting);
        }
        let mut resource = self.inner().resource();
        if !matches!(*resource, Resource::Unbound) {
            return Err(RegistryError::AlreadyBound);
        }
        *resource = Resource::Bound(Box::new(close));
        Ok(())
    }

    pub fn end(&self) {
        self.inner().end();
    }

    /// Go `EndWithRelease`: closes the resource, bumps the release sequence of an
    /// accounted scope and returns the acknowledgement ticket. Idempotent.
    pub fn end_with_release(&self) -> Option<ReleaseTicket> {
        self.inner().end()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    fn spec(credential: &str, accounted: bool) -> ScopeSpec {
        ScopeSpec {
            request_id: "req".into(),
            credential_id: credential.into(),
            model: "gpt".into(),
            kind: "http".into(),
            started_at: SystemTime::UNIX_EPOCH,
            accounted,
        }
    }

    #[test]
    fn dropped_pending_tokens_end_and_installed_ones_become_scopes() {
        let registry = Registry::new();
        let pending = registry.begin_dispatch().unwrap();
        assert_eq!(registry.pending(), 1);
        drop(pending);
        assert_eq!(registry.pending(), 0);
        let scope = registry
            .install(registry.begin_dispatch().unwrap(), spec("a", false))
            .unwrap();
        assert_eq!((registry.pending(), registry.active()), (0, 1));
        scope.end();
        scope.end();
        assert_eq!(registry.active(), 0);
    }

    #[test]
    fn a_token_from_another_registry_is_rejected_and_ended() {
        let (a, b) = (Registry::new(), Registry::new());
        let pending = a.begin_dispatch().unwrap();
        assert_eq!(
            b.install(pending, spec("x", false)).err(),
            Some(RegistryError::InvalidResource)
        );
        assert_eq!(a.pending(), 0);
    }

    /// Go `TestRegistryEndMarksOneDirtyGroup`, `TestUnaccountedScopeDoesNotRelease` and
    /// `TestSetReleaseSinkReplaysExistingSequences`: one increment per accounted scope
    /// end, none for an unaccounted one, and a new sink gets every group's latest.
    #[test]
    fn accounted_scopes_release_cumulative_sequences_per_group_once() {
        let registry = Registry::new();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let sink_seen = seen.clone();
        registry.set_release_sink(Some(Arc::new(move |group: ReleaseGroup, sequence| {
            sink_seen.lock().unwrap().push((group.credential_id, sequence));
            None
        })));
        let first = registry
            .install(registry.begin_dispatch().unwrap(), spec("a", true))
            .unwrap();
        let second = registry
            .install(registry.begin_dispatch().unwrap(), spec("a", true))
            .unwrap();
        let other = registry
            .install(registry.begin_dispatch().unwrap(), spec("b", true))
            .unwrap();
        let unaccounted = registry
            .install(registry.begin_dispatch().unwrap(), spec("a", false))
            .unwrap();
        first.end();
        first.end();
        unaccounted.end();
        second.end();
        other.end();
        assert_eq!(
            *seen.lock().unwrap(),
            vec![("a".to_owned(), 1), ("a".to_owned(), 2), ("b".to_owned(), 1)]
        );
        // A replacement sink receives the latest sequence of every group.
        let replayed = Arc::new(Mutex::new(Vec::new()));
        let replay_seen = replayed.clone();
        registry.set_release_sink(Some(Arc::new(move |group: ReleaseGroup, sequence| {
            replay_seen.lock().unwrap().push((group.credential_id, sequence));
            None
        })));
        let mut replayed = replayed.lock().unwrap().clone();
        replayed.sort();
        assert_eq!(replayed, vec![("a".to_owned(), 2), ("b".to_owned(), 1)]);
    }

    /// Go `TestScopeEndIsExactlyOnce`.
    #[test]
    fn binding_is_single_and_end_closes_the_resource() {
        let registry = Registry::new();
        let scope = registry
            .install(registry.begin_dispatch().unwrap(), spec("a", false))
            .unwrap();
        let closed = Arc::new(AtomicUsize::new(0));
        let counter = closed.clone();
        scope
            .bind(move || {
                counter.fetch_add(1, Ordering::SeqCst);
            })
            .unwrap();
        assert_eq!(scope.bind(|| {}), Err(RegistryError::AlreadyBound));
        scope.end();
        scope.end();
        assert_eq!(closed.load(Ordering::SeqCst), 1);
        assert_eq!(scope.bind(|| {}), Err(RegistryError::NotAccepting));
    }

    /// Go `TestDrainRejectsLateInstallAndCancelsBoundScopes`.
    #[tokio::test]
    async fn drain_cancels_resources_rejects_work_and_waits_for_owners() {
        let registry = Registry::new();
        let scope = registry
            .install(registry.begin_dispatch().unwrap(), spec("a", true))
            .unwrap();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        scope
            .bind(move || {
                let _ = tx.send(());
            })
            .unwrap();
        let owner = tokio::spawn({
            let scope = scope.clone();
            async move {
                rx.recv().await;
                scope.end();
            }
        });
        registry.drain(Duration::from_secs(1)).await.unwrap();
        owner.await.unwrap();
        assert_eq!(registry.begin_dispatch().err(), Some(RegistryError::NotAccepting));
        assert_eq!(registry.drain(Duration::from_secs(1)).await, Err(RegistryError::Closed));
    }

    #[tokio::test]
    async fn drain_times_out_while_a_dispatch_is_unresolved() {
        let registry = Registry::new();
        let pending = registry.begin_dispatch().unwrap();
        assert_eq!(
            registry.drain(Duration::from_millis(20)).await,
            Err(RegistryError::Timeout)
        );
        // Installing after drain started is refused and resolves the token.
        assert_eq!(
            registry.install(pending, spec("a", false)).err(),
            Some(RegistryError::NotAccepting)
        );
        assert_eq!(registry.pending(), 0);
    }

    /// Go `TestDrainReturnsWhenBlockingResourceCloseExceedsContext`.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn release_waits_for_a_close_in_progress_and_drain_stays_bounded() {
        let registry = Registry::new();
        let released = Arc::new(Mutex::new(Vec::new()));
        let seen = released.clone();
        registry.set_release_sink(Some(Arc::new(move |_group: ReleaseGroup, sequence| {
            seen.lock().unwrap().push(sequence);
            None
        })));
        let scope = registry
            .install(registry.begin_dispatch().unwrap(), spec("a", true))
            .unwrap();
        let (gate_tx, gate_rx) = std::sync::mpsc::channel::<()>();
        let (started_tx, started_rx) = std::sync::mpsc::channel::<()>();
        scope
            .bind(move || {
                let _ = started_tx.send(());
                let _ = gate_rx.recv();
            })
            .unwrap();
        // Drain starts the slow close and still honours its bound.
        assert_eq!(
            registry.drain(Duration::from_millis(50)).await,
            Err(RegistryError::Timeout)
        );
        started_rx.recv().unwrap();
        // An end during the close must not release before the close finished.
        let ender = std::thread::spawn({
            let scope = scope.clone();
            move || scope.end()
        });
        std::thread::sleep(Duration::from_millis(50));
        assert!(released.lock().unwrap().is_empty());
        assert_eq!(registry.active(), 1);
        gate_tx.send(()).unwrap();
        ender.join().unwrap();
        assert_eq!(*released.lock().unwrap(), vec![1]);
        registry.drain(Duration::from_secs(1)).await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn drain_waits_until_the_release_sink_returns() {
        let registry = Registry::new();
        let (enter_tx, enter_rx) = std::sync::mpsc::channel::<()>();
        let (leave_tx, leave_rx) = std::sync::mpsc::channel::<()>();
        let leave_rx = Mutex::new(leave_rx);
        registry.set_release_sink(Some(Arc::new(move |_group: ReleaseGroup, _sequence| {
            let _ = enter_tx.send(());
            let _ = leave_rx.lock().unwrap().recv();
            None
        })));
        let scope = registry
            .install(registry.begin_dispatch().unwrap(), spec("a", true))
            .unwrap();
        let ender = std::thread::spawn(move || scope.end());
        enter_rx.recv().unwrap();
        assert_eq!(
            registry.drain(Duration::from_millis(50)).await,
            Err(RegistryError::Timeout)
        );
        leave_tx.send(()).unwrap();
        ender.join().unwrap();
        registry.drain(Duration::from_secs(1)).await.unwrap();
    }

    #[test]
    fn dropping_the_last_handle_ends_the_scope() {
        let registry = Registry::new();
        let released = Arc::new(Mutex::new(0));
        let count = released.clone();
        registry.set_release_sink(Some(Arc::new(move |_group: ReleaseGroup, _sequence| {
            *count.lock().unwrap() += 1;
            None
        })));
        let scope = registry
            .install(registry.begin_dispatch().unwrap(), spec("a", true))
            .unwrap();
        let clone = scope.clone();
        drop(scope);
        assert_eq!(registry.active(), 1);
        drop(clone);
        assert_eq!((registry.active(), *released.lock().unwrap()), (0, 1));
    }

    /// Go `TestFreezeInFlightWaitsForPendingBarrierAndCopiesScopes` (a Rust freeze owns
    /// its observations, so Go's copy check holds by construction).
    #[test]
    fn barrier_publishes_only_after_older_dispatches_resolve() {
        let registry = Registry::new();
        let older = registry.begin_dispatch().unwrap();
        registry.observe_barrier(5);
        let newer = registry.begin_dispatch().unwrap();
        assert_eq!(registry.freeze().barrier_revision, 0);
        drop(older);
        let freeze = registry.freeze();
        assert_eq!((freeze.revision, freeze.barrier_revision), (2, 5));
        drop(newer);
        registry.observe_barrier(3);
        assert_eq!(registry.freeze().barrier_revision, 5);
    }

    /// Go `TestDrainWaitsForPendingDispatch`, `TestDrainRejectsLateInstall` and
    /// `TestWaitPendingDoesNotDrainActiveScope`: a drain waits for an unresolved
    /// dispatch, which can no longer install; waiting for pending dispatches ignores
    /// active scopes and keeps the registry accepting.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn drains_and_pending_waits_wait_for_unresolved_dispatches() {
        let registry = Registry::new();
        let pending = registry.begin_dispatch().unwrap();
        let drain = tokio::spawn({
            let registry = registry.clone();
            async move { registry.drain(Duration::from_secs(1)).await }
        });
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert!(!drain.is_finished(), "the dispatch is unresolved");
        assert_eq!(
            registry.install(pending, spec("a", false)).err(),
            Some(RegistryError::NotAccepting)
        );
        assert_eq!(drain.await.unwrap(), Ok(()));

        let registry = Registry::new();
        let active = registry
            .install(registry.begin_dispatch().unwrap(), spec("a", false))
            .unwrap();
        let pending = registry.begin_dispatch().unwrap();
        let wait = tokio::spawn({
            let registry = registry.clone();
            async move { registry.wait_pending(Duration::from_secs(1)).await }
        });
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert!(!wait.is_finished(), "the dispatch is unresolved");
        drop(pending);
        assert_eq!(wait.await.unwrap(), Ok(()));
        assert!(registry.begin_dispatch().is_ok(), "still accepting");
        active.end();
    }

    /// Go `TestDrainWaitsForBlockingResourceClose`,
    /// `TestConcurrentDrainWaitsForBlockingResourceClose`,
    /// `TestConcurrentCloseWaitsForBlockingResourceClose` and `TestDrainRejectsLateBind`:
    /// drains, closes and ends all wait for a close in progress, and a draining registry
    /// refuses a new binding.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn drains_closes_and_ends_wait_for_a_blocking_close() {
        type Blocking = (Scope, std::sync::mpsc::Receiver<()>, std::sync::mpsc::Sender<()>);
        let blocking = |registry: &Registry| -> Blocking {
            let scope = registry
                .install(registry.begin_dispatch().unwrap(), spec("a", false))
                .unwrap();
            let (started_tx, started_rx) = std::sync::mpsc::channel::<()>();
            let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
            scope
                .bind(move || {
                    let _ = started_tx.send(());
                    let _ = release_rx.recv();
                })
                .unwrap();
            (scope, started_rx, release_tx)
        };
        let settle = || std::thread::sleep(Duration::from_millis(20));

        let registry = Registry::new();
        let (scope, started, release) = blocking(&registry);
        let drain = |registry: &Registry| {
            let registry = registry.clone();
            tokio::spawn(async move { registry.drain(Duration::from_secs(2)).await })
        };
        let first = drain(&registry);
        started.recv().unwrap();
        let ender = std::thread::spawn(move || scope.end());
        let second = drain(&registry);
        settle();
        assert!(!first.is_finished() && !second.is_finished() && !ender.is_finished());
        release.send(()).unwrap();
        ender.join().unwrap();
        assert_eq!(first.await.unwrap(), Ok(()));
        assert_eq!(second.await.unwrap(), Ok(()));

        let registry = Registry::new();
        let (scope, started, release) = blocking(&registry);
        let close = |registry: &Registry| {
            let registry = registry.clone();
            std::thread::spawn(move || registry.close())
        };
        let first = close(&registry);
        started.recv().unwrap();
        let second = close(&registry);
        settle();
        assert!(!first.is_finished() && !second.is_finished());
        release.send(()).unwrap();
        first.join().unwrap();
        second.join().unwrap();
        drop(scope);

        let registry = Registry::new();
        let scope = registry
            .install(registry.begin_dispatch().unwrap(), spec("a", false))
            .unwrap();
        let drain = drain(&registry);
        while registry.accepting() {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        assert_eq!(scope.bind(|| {}), Err(RegistryError::NotAccepting));
        scope.end();
        assert_eq!(drain.await.unwrap(), Ok(()));
    }
}
