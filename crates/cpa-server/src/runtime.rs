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
use cpa_exec::{Executors, Readiness};
use futures_util::{Stream, StreamExt};
use serde_json::{Map, Value};

use crate::refresh::RefreshState;
use crate::registry::Registry;
use crate::scheduler::{Policy, Scheduler};

/// The config snapshot and credential epoch a registry was built from.
type RegistryCache = (Arc<Config>, (u64, u64), Arc<Registry>);

pub struct Runtime {
    config: RwLock<Arc<Config>>,
    store: Arc<CredentialStore>,
    pub executors: Executors,
    refresh_task: Mutex<Option<tokio::task::AbortHandle>>,
    refresh_state: Mutex<RefreshState>,
    /// The registry derived from the current config and credential generation.
    registry: Mutex<Option<RegistryCache>>,
    oauth_sink: RwLock<Option<OAuthCallbackSink>>,
    /// Go `modelPoolOffsets`: rotation cursors for OpenAI-compatible alias pools.
    pool_offsets: Mutex<HashMap<String, usize>>,
    /// Usage records for `GET /observability/usage/queue` (management configures it).
    usage: Arc<crate::usage::UsageQueue>,
    /// `--local-model`: embedded model catalogs only, no remote catalog refresh.
    local_model: std::sync::atomic::AtomicBool,
    /// The native plugin host, synced with each published config (see [`crate::plugins`]).
    plugins: crate::plugins::PluginRuntime,
    /// The remote dispatcher that replaces local selection (Go Home mode).
    remote: RwLock<Option<Arc<dyn crate::remote::RemoteDispatch>>>,
    /// Set only by [`crate::testing::runtime`]: executor calls get credentials that
    /// cannot leave the machine.
    pub(crate) deny_external: std::sync::atomic::AtomicBool,
    /// Marked on every config publish, so background loops sleep until it changes.
    config_changed: tokio::sync::watch::Sender<()>,
}

/// An OAuth provider redirect received on the main listener.
#[derive(Debug, Clone)]
pub struct OAuthCallback {
    /// `anthropic`, `codex`, `antigravity` or `devin`.
    pub provider: &'static str,
    pub state: String,
    pub code: String,
    pub error: String,
}

/// Hands a callback to a pending management login (Go
/// `WriteOAuthCallbackFileForPendingSession`); returns false when no login with that
/// state is pending or the callback could not be recorded.
pub type OAuthCallbackSink = Arc<dyn Fn(&OAuthCallback) -> bool + Send + Sync>;

impl Runtime {
    /// The scheduler policy is derived from the config (`routing` plus the OAuth
    /// provider rules), as on every publish.
    pub fn new(config: Config, credentials: Vec<Credential>, executors: Executors) -> Self {
        let mut policy = crate::management::policy(&config);
        policy.compat_disable_cooling = crate::scheduler::compat_cooling(&config);
        let cooldown_dir = cooldown_dir(&config, &policy);
        let (enabled, strict) = signature_cache_config(&config);
        cpa_translate::set_antigravity_signature_cache_config(enabled, strict);
        let rt = Self {
            config: RwLock::new(Arc::new(config)),
            store: CredentialStore::new(credentials),
            executors,
            refresh_task: Mutex::default(),
            refresh_state: Mutex::default(),
            registry: Mutex::default(),
            oauth_sink: RwLock::default(),
            pool_offsets: Mutex::default(),
            usage: Arc::default(),
            local_model: Default::default(),
            plugins: Default::default(),
            remote: RwLock::default(),
            deny_external: Default::default(),
            config_changed: tokio::sync::watch::Sender::new(()),
        };
        rt.publish_policy(policy);
        rt.store.configure_cooldown_store(cooldown_dir);
        // Failed attempts publish Go's error events on the RESP `errors` channel.
        let _ = rt.store.error_events.set(rt.usage.clone());
        rt
    }

    /// The usage queue: the request path calls `enqueue` with one serialized Go
    /// `queuedUsageDetail` per upstream request when `accepts()` (additive API).
    pub fn usage_queue(&self) -> &crate::usage::UsageQueue {
        &self.usage
    }

    /// Records `--local-model` (Go `modelCatalogUpdaterPlan`'s `localModel`): remote
    /// model catalog updaters must not start when set (additive API).
    pub fn set_local_model(&self, on: bool) {
        self.local_model.store(on, std::sync::atomic::Ordering::Relaxed);
    }

    /// Persists cooldowns to `backend` instead of `.cds` files (PGSTORE's cooldown
    /// table; additive API). Call before serving.
    pub fn set_cooldown_backend(&self, backend: Arc<dyn crate::cooldown_store::Backend>) {
        self.store.set_cooldown_backend(backend);
    }

    /// Whether `--local-model` was given.
    pub fn local_model(&self) -> bool {
        self.local_model.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Serves with `host`, a plugin host that already runs (Go hands the host that
    /// registered plugin command-line flags to the service; additive API). Call before
    /// [`crate::plugins::start`].
    pub fn with_plugin_host(mut self, host: cpa_plugin::Host) -> Self {
        self.plugins = crate::plugins::PluginRuntime::with_host(host);
        self
    }

    /// The plugin host (additive API). [`crate::plugins::start`] syncs it with the
    /// config.
    pub fn plugins(&self) -> &cpa_plugin::Host {
        self.plugins.host()
    }

    pub(crate) fn plugin_runtime(&self) -> &crate::plugins::PluginRuntime {
        &self.plugins
    }

    /// The config snapshot to use for one whole request.
    pub fn config(&self) -> Arc<Config> {
        self.config.read().unwrap_or_else(PoisonError::into_inner).clone()
    }

    /// Resolves after the next config publish.
    pub fn subscribe_config(&self) -> tokio::sync::watch::Receiver<()> {
        self.config_changed.subscribe()
    }

    /// Replaces the config and the scheduler policy derived from it. Requests already
    /// running keep their snapshot.
    pub fn publish_config(&self, config: Config) {
        let policy = crate::management::policy(&config);
        self.publish_config_and_policy(config, policy);
    }

    /// Integration should use this when publishing parsed routing settings too.
    pub fn publish_config_and_policy(&self, config: Config, mut policy: Policy) {
        policy.compat_disable_cooling = crate::scheduler::compat_cooling(&config);
        let (enabled, strict) = signature_cache_config(&config);
        cpa_translate::set_antigravity_signature_cache_config(enabled, strict);
        let dir = cooldown_dir(&config, &policy);
        let ws_auth = crate::relay::ws_auth(&config);
        let config = Arc::new(config);
        let previous = {
            // The plugin worker is told under the same lock, so concurrent publishes
            // reach it in the order they replaced the config.
            let mut current = self.config.write().unwrap_or_else(PoisonError::into_inner);
            let previous = std::mem::replace(&mut *current, config.clone());
            self.plugins.config_published(config);
            previous
        };
        // Go's websocket auth change handler: turning ws-auth on ends the relay sessions
        // that connected without a key.
        if ws_auth && !crate::relay::ws_auth(&previous) {
            self.executors.google.aistudio.relay.stop();
        }
        self.publish_policy(policy);
        self.store.configure_cooldown_store(dir);
        self.config_changed.send_replace(());
    }

    /// A coherent config/policy pair for the entire route attempt loop.
    pub fn request_snapshot(&self) -> (Arc<Config>, Arc<Policy>) {
        let config = self.config.read().unwrap_or_else(PoisonError::into_inner);
        (config.clone(), self.policy())
    }

    pub fn store(&self) -> &Arc<CredentialStore> {
        &self.store
    }

    /// Go `nextModelPoolOffset`: the start index for the next pass over a pool.
    pub fn next_pool_offset(&self, key: &str, size: usize) -> usize {
        if size <= 1 || key.trim().is_empty() {
            return 0;
        }
        let key = key.trim();
        let mut offsets = self.pool_offsets.lock().unwrap_or_else(PoisonError::into_inner);
        // ponytail: Go never prunes, so keys of removed credentials would accumulate
        // forever. Past 4096 keys the map is cleared, which restarts rotation for every
        // pool from offset 0. Upgrade path: drop a pool's key when its credential set
        // changes in reconcile.
        if offsets.len() >= 4096 && !offsets.contains_key(key) {
            offsets.clear();
        }
        let slot = offsets.entry(key.to_owned()).or_default();
        let offset = if *slot >= 2_147_483_640 { 0 } else { *slot };
        *slot = offset + 1;
        offset % size
    }

    /// One credential's projected state for one registered model (Go
    /// `clientModelProjectionForAuth`), for listings and `auto`.
    pub fn suspension(&self, credential_id: &str, model: &str) -> crate::registry::Suspension {
        use crate::registry::Suspension;
        let Some(credential) = self.store.get(credential_id) else {
            return Suspension::None;
        };
        let aliases = crate::registry::global_aliases(&self.config());
        let key = crate::registry::selection_model(&aliases, &credential, model);
        self.store.suspension(&credential, &key)
    }

    /// Wires OAuth callback delivery (management owns pending login sessions).
    pub fn set_oauth_callback_sink(&self, sink: Option<OAuthCallbackSink>) {
        *self.oauth_sink.write().unwrap_or_else(PoisonError::into_inner) = sink;
    }

    pub fn deliver_oauth_callback(&self, callback: &OAuthCallback) -> bool {
        let sink = self.oauth_sink.read().unwrap_or_else(PoisonError::into_inner).clone();
        sink.is_some_and(|sink| sink(callback))
    }

    /// The credential an executor call receives: the stored one, or in a test runtime
    /// ([`crate::testing`]) a copy that cannot leave the machine.
    pub(crate) fn for_executor(&self, credential: &Arc<Credential>) -> Arc<Credential> {
        if self.deny_external.load(std::sync::atomic::Ordering::Relaxed) {
            crate::testing::guarded(credential, &self.config())
        } else {
            credential.clone()
        }
    }

    /// The model registry for the current config and credential set. Rebuilt only when
    /// either changed, so it is always derived, never separately maintained.
    pub fn registry(&self) -> Arc<Registry> {
        let config = self.config();
        // A refreshed static catalog re-registers every credential (Go's model refresh
        // callback), so the catalog generation is part of the key.
        let generation = (self.store.epoch(), cpa_core::registry::catalog_generation());
        let mut cache = self.registry.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some((cfg, g, registry)) = cache.as_ref()
            && Arc::ptr_eq(cfg, &config)
            && *g == generation
        {
            return registry.clone();
        }
        let registry = Arc::new(Registry::build(&config, &self.store.snapshot()));
        *cache = Some((config, generation, registry.clone()));
        registry
    }

    pub fn policy(&self) -> Arc<Policy> {
        self.store.policy.read().unwrap_or_else(PoisonError::into_inner).clone()
    }

    pub fn publish_policy(&self, policy: Policy) {
        {
            let mut current = self.store.policy.write().unwrap_or_else(PoisonError::into_inner);
            self.store
                .scheduler
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .configure(&current, &policy);
            *current = Arc::new(policy);
        }
        self.store.clear_disabled_cooldowns();
    }

    /// Selects a credential that registered the route model. Waits for preparation only
    /// when the executor reports [`Readiness::PrepareNow`] (single-flighted per
    /// credential); token refreshes stay with the background loop. The returned lease
    /// holds the committed snapshot.
    pub async fn acquire(
        &self,
        selection: Selection,
        cfg: &Config,
        policy: Arc<Policy>,
        registry: &Registry,
    ) -> Result<Lease, AcquireError> {
        let aliases = crate::registry::global_aliases(cfg);
        let scope = selection.clone();
        let admit = admission(registry, &aliases, &scope, &self.executors);
        // `soonest-reset` ranks by the quota windows the executors last observed.
        let now = std::time::SystemTime::now();
        let ranks = |c: &Credential| {
            let provider = c.provider.trim().to_ascii_lowercase();
            let snapshot = match provider.as_str() {
                "claude" => self.executors.claude.quota().snapshot(&c.id),
                "codex" => self.executors.codex.quota().snapshot(&c.id),
                _ => None,
            };
            snapshot.map_or_else(crate::scheduler::Windows::default, |s| {
                crate::scheduler::Windows::observed(&provider, &s.signals, s.observed_at, now)
            })
        };
        let mut lease = self.store.select_ranked(selection, policy, &admit, &ranks)?;
        if self.executors.readiness(&lease.credential, cfg) != Readiness::PrepareNow {
            return Ok(lease);
        }
        let id = lease.credential.id.clone();
        match self.prepare_credential(&id, cfg, None).await {
            Ok(current) => lease.credential = current,
            Err(error) => {
                lease.complete(Outcome::Failure(error.clone()));
                return Err(AcquireError::Prepare { id, error });
            }
        }
        Ok(lease)
    }

    /// The installed remote dispatcher (Go Home mode), if any.
    pub fn remote_dispatch(&self) -> Option<Arc<dyn crate::remote::RemoteDispatch>> {
        self.remote.read().unwrap_or_else(PoisonError::into_inner).clone()
    }

    /// Routes every request through `dispatch` (Go Home mode) or back to the local
    /// scheduler with `None` (additive API).
    pub fn set_remote_dispatch(&self, dispatch: Option<Arc<dyn crate::remote::RemoteDispatch>>) {
        *self.remote.write().unwrap_or_else(PoisonError::into_inner) = dispatch;
    }

    /// One remote pick (Go `pickHomeDispatchSelection`): the lease ends through the
    /// dispatcher, and its release joins `releases`. Also returns Home's request-retry
    /// limit and the client key Home authenticated (empty when it sent none).
    pub(crate) async fn acquire_remote(
        &self,
        dispatch: &dyn crate::remote::RemoteDispatch,
        selection: Selection,
        request: crate::remote::RemoteRequest,
        releases: &crate::remote::PendingReleases,
    ) -> Result<(Lease, Option<i64>, String), crate::remote::RemoteError> {
        let grant = dispatch.dispatch(request).await?;
        let lease = Lease {
            store: self.store.clone(),
            credential: Arc::new(grant.credential),
            execution_model: selection.model.clone(),
            selection,
            attempt: self.store.attempts.fetch_add(1, Ordering::Relaxed),
            policy: self.policy(),
            reported: false,
            picked: 0,
            remote: Some(crate::remote::RemoteEnd {
                end: Some(grant.end),
                releases: releases.clone(),
                cancel: grant.cancel,
            }),
            // ponytail: Go's Home dispatch keys its own session aliases on LCP matches
            // (home_session_alias.go); remote picks here carry no LCP binding.
            lcp: None,
        };
        Ok((lease, grant.request_retry, grant.user_api_key))
    }

    /// Go `prepareHomeRequestAuth`: a dispatched credential that requests must wait for
    /// (a Claude identity, a Meta mint) is prepared on this attempt's copy only,
    /// serialized per credential ID. Home owns the stored credential, so nothing is
    /// committed locally.
    pub(crate) async fn prepare_remote(&self, lease: &mut Lease, cfg: &Config) -> Result<(), ExecError> {
        if self.executors.readiness(&lease.credential, cfg) != Readiness::PrepareNow {
            return Ok(());
        }
        let lock = self.store.prepare_lock(&lease.credential.id);
        let _guard = lock.lock().await;
        let patch = self
            .executors
            .prepare(&self.for_executor(&lease.credential), cfg)
            .await?;
        let mut prepared = (*lease.credential).clone();
        patch.apply(&mut prepared.metadata);
        lease.credential = Arc::new(prepared);
        Ok(())
    }

    /// Prepares and commits one credential. `failed` is the revision whose token an
    /// upstream rejected: preparation then runs even when not due, unless another task
    /// already replaced that revision (Go `refreshAuthForRequest`).
    async fn prepare_credential(
        &self,
        id: &str,
        cfg: &Config,
        failed: Option<u64>,
    ) -> Result<Arc<Credential>, ExecError> {
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
        match failed {
            Some(revision) if current.revision != revision => return Ok(current),
            Some(_) => {}
            None if self.executors.readiness(&current, cfg) == Readiness::Ready => return Ok(current),
            None => {}
        }
        let patch = self.executors.prepare(&self.for_executor(&current), cfg).await?;
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

    /// Go `tryRefreshAfterUnauthorized`: after an upstream 401 on a credential whose
    /// executor reports a refresh credential, prepare it once (due or not) and return the
    /// committed replacement to retry with.
    // ponytail: refresh goes through `Executors::prepare`; a provider whose prepare only
    // refreshes inside its lead (Claude) cannot recover a revoked but unexpired token.
    pub async fn refresh_after_unauthorized(&self, credential: &Credential, cfg: &Config) -> Option<Arc<Credential>> {
        if !self.executors.has_refresh_credential(credential) {
            return None;
        }
        let refreshed = self
            .prepare_credential(&credential.id, cfg, Some(credential.revision))
            .await
            .ok()?;
        (refreshed.revision != credential.revision).then_some(refreshed)
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
        // Wakes at the next deadline, or at the latest after `HORIZON` of awake time while
        // a credential can ever come due, to catch wall-clock jumps such as a resumed
        // laptop (monotonic time stops during suspend). Credentials that never need
        // preparation (API keys, Gemini CLI tokens) set no timer.
        const HORIZON: Duration = Duration::from_secs(600);
        // Store changes (each committed refresh is one) are coalesced over this delay.
        const COALESCE: Duration = Duration::from_secs(1);
        let weak = Arc::downgrade(self);
        let mut store_changes = self.store.subscribe();
        let mut config_changes = self.subscribe_config();
        let handle = tokio::spawn(async move {
            loop {
                let Some(rt) = weak.upgrade() else {
                    break;
                };
                // Everything published so far is in this scan, including the commits of
                // the previous batch.
                store_changes.borrow_and_update();
                config_changes.borrow_and_update();
                let cfg = rt.config();
                let snapshot = rt.store.snapshot();
                let (now, wall) = (Instant::now(), chrono::Utc::now());
                let horizon = chrono::Duration::from_std(HORIZON).unwrap_or_default();
                let mut wake: Option<Instant> = None;
                let mut soonest = |at: Instant| wake = Some(wake.map_or(at, |w| w.min(at)));
                let jobs: Vec<_> = {
                    let mut state = rt.refresh_state.lock().unwrap_or_else(PoisonError::into_inner);
                    state.reconcile(&snapshot);
                    let mut jobs = Vec::new();
                    // Far enough ahead that any expiry has passed, near enough for chrono.
                    let eventually = wall + chrono::Duration::days(36_500);
                    for c in snapshot.iter().filter(|c| refresh_candidate(c)) {
                        if rt.executors.needs_prepare_at(c, &cfg, eventually) {
                            soonest(now + HORIZON);
                        }
                        match rt.executors.prepare_due(c, &cfg, wall, horizon) {
                            Some(at) if at > wall => soonest(now + (at - wall).to_std().unwrap_or_default()),
                            Some(_) if state.reserve(c, now) => jobs.push(c.clone()),
                            // Reserved or backing off: look again when that ends.
                            Some(_) => {
                                if let Some(at) = state.retry_at(c).filter(|at| *at > now) {
                                    soonest(at);
                                }
                            }
                            None => {}
                        }
                    }
                    jobs
                };
                let busy = !jobs.is_empty();
                // ponytail: bounded batches, so a slow worker delays the next scan.
                // Use an independent queue if refresh latency matters for large pools.
                futures_util::stream::iter(jobs)
                    .for_each_concurrent(refresh_workers(&cfg), |credential| {
                        let rt = rt.clone();
                        let cfg = cfg.clone();
                        async move {
                            let result = rt.prepare_credential(&credential.id, &cfg, None).await;
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
                            let ineffective =
                                result.is_ok() && rt.executors.readiness(&current, &cfg) != Readiness::Ready;
                            rt.refresh_state.lock().unwrap_or_else(PoisonError::into_inner).finish(
                                &current,
                                result.as_ref().err(),
                                ineffective,
                                Instant::now(),
                            );
                        }
                    })
                    .await;
                drop(rt);
                // After a batch, look again at once: finished jobs set their backoff,
                // and the next scan turns it into a deadline.
                if busy {
                    continue;
                }
                let deadline = async {
                    match wake {
                        Some(at) => tokio::time::sleep_until(at.into()).await,
                        None => std::future::pending().await,
                    }
                };
                tokio::select! {
                    () = deadline => {}
                    changed = store_changes.changed() => {
                        if changed.is_err() {
                            break;
                        }
                        tokio::time::sleep(COALESCE).await;
                    }
                    changed = config_changes.changed() => {
                        if changed.is_err() {
                            break;
                        }
                        tokio::time::sleep(COALESCE).await;
                    }
                }
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

/// Where cooldowns persist: `auth-dir` while `save-cooldown-status` is on.
fn cooldown_dir(cfg: &Config, policy: &Policy) -> Option<std::path::PathBuf> {
    policy.save_cooldown_status.then(|| cfg.auth_dir.clone())
}

/// `oauth.auth-auto-refresh-workers`: non-positive means Go's default of 16.
fn refresh_workers(cfg: &Config) -> usize {
    cfg.document
        .get("oauth")
        .and_then(|o| o.get("auth-auto-refresh-workers"))
        .and_then(serde_yaml_ng::Value::as_i64)
        .filter(|n| *n > 0)
        .map_or(16, |n| n as usize)
}

#[derive(Debug)]
pub enum AcquireError {
    /// No candidate is ready. `retry_after` is set when candidates exist but are all
    /// blocked (Go `auth_unavailable`); unset means none is eligible (`auth_not_found`).
    Unavailable {
        retry_after: Option<Duration>,
        cause: Option<String>,
    },
    /// Every eligible candidate is quota-cooling for this model.
    Cooldown {
        wait: Duration,
        cause: Option<String>,
    },
    Prepare {
        id: String,
        error: ExecError,
    },
}

/// Route-model state is separate from the lease's resolved execution model.
#[derive(Debug, Clone, Default)]
pub struct Selection {
    /// Provider keys that may serve the model, in preference order.
    pub providers: Vec<String>,
    /// Single-provider shorthand, used when `providers` is empty.
    pub provider: String,
    /// The route model (requested model after `auto` resolution, suffix kept).
    pub model: String,
    /// The session this request binds to (Go `extractSessionIDs` primary).
    pub session: Option<String>,
    /// The session's parent, or the conversation alias of a prompt-cache-key session
    /// (Go fallback ID).
    pub session_parent: Option<String>,
    /// The session forked from `session_parent` (inherits its binding).
    pub session_fork: bool,
    /// A request without an explicit session, for the LCP matcher (Go `pickLCP`). Set
    /// only with session affinity on; it then decides the binding instead of `session`.
    pub lcp: Option<Arc<crate::lcp::Request>>,
    /// Credential IDs already tried in this request.
    pub exclude: Vec<String>,
    pub retry_round: usize,
}

impl Selection {
    pub fn new(provider: &str, model: &str) -> Self {
        Self {
            providers: vec![provider.to_owned()],
            model: model.to_owned(),
            ..Self::default()
        }
    }

    /// The provider keys this selection may use.
    pub fn provider_keys(&self) -> Vec<String> {
        if self.providers.is_empty() && !self.provider.is_empty() {
            vec![self.provider.clone()]
        } else {
            self.providers.clone()
        }
    }
}

/// Whether `next` would change nothing about the stored `current` (reconcile keeps
/// such a credential and its revision).
fn unchanged(current: &Credential, next: &Credential) -> bool {
    current.id == next.id
        && current.source == next.source
        && current.metadata == next.metadata
        && current.attributes == next.attributes
        && current.provider == next.provider
        && current.disabled == next.disabled
        && current.label == next.label
}

/// Admission without a registry: provider match, prefix, config aliases and the
/// credential's exclusions (attributes first, then file metadata).
pub fn standalone_admission<'a>(
    selection: &'a Selection,
    policy: &'a Policy,
) -> impl Fn(&Credential) -> Option<String> + 'a {
    let providers = selection.provider_keys();
    move |c| {
        if !providers.contains(&crate::registry::provider_key(c)) {
            return None;
        }
        let route = selection.model.trim();
        let prefix = crate::registry::credential_prefix(c);
        let base = crate::scheduler::canonical_model(route);
        if !prefix.is_empty()
            && !base.starts_with(&format!("{prefix}/"))
            && (policy.force_model_prefix || base.contains('/'))
        {
            return None;
        }
        let (models, _) = crate::registry::execution_models(&HashMap::new(), c, route);
        let model = models.into_iter().next().unwrap_or_default();
        let key = crate::scheduler::canonical_model(&model).to_lowercase();
        let excluded: Vec<String> = match c.attributes.get("excluded_models").filter(|v| !v.trim().is_empty()) {
            Some(list) => list.split(',').map(|p| p.trim().to_lowercase()).collect(),
            None => c
                .metadata
                .get("excluded_models")
                .and_then(serde_json::Value::as_array)
                .map(|l| {
                    l.iter()
                        .filter_map(|v| v.as_str())
                        .map(|p| p.trim().to_lowercase())
                        .collect()
                })
                .unwrap_or_default(),
        };
        if excluded.iter().any(|p| crate::registry::wildcard(p, &key)) {
            return None;
        }
        Some(model)
    }
}

#[derive(Debug, Clone)]
pub enum Outcome {
    Success,
    Failure(ExecError),
    /// A failure that must not change availability (compact request faults, a
    /// missing count_tokens endpoint).
    Neutral(ExecError),
    /// The client went away before the response finished.
    Cancelled,
}

/// Decides which credentials may serve a selection and under which model key their
/// cooldowns live. `None` rejects the credential.
pub type Admit<'a> = dyn Fn(&Credential) -> Option<String> + 'a;

/// Registry admission (Go `authSupportsRouteModel`) plus a registered executor.
pub fn admission<'a>(
    registry: &'a Registry,
    aliases: &'a HashMap<String, Vec<crate::registry::OAuthAlias>>,
    selection: &'a Selection,
    executors: &'a Executors,
) -> impl Fn(&Credential) -> Option<String> + 'a {
    move |c| {
        let provider = crate::registry::provider_key(c);
        if !selection.provider_keys().contains(&provider) || !executors.supports(&c.provider) {
            return None;
        }
        let key = crate::registry::selection_model(aliases, c, &selection.model);
        let route = crate::scheduler::canonical_model(&selection.model);
        if route.is_empty() {
            return Some(key);
        }
        let selection_key = crate::scheduler::canonical_model(&key);
        (registry.client_supports(&c.id, route)
            || (selection_key != route && registry.client_supports(&c.id, selection_key)))
        .then_some(key)
    }
}

/// One attempt with one credential. Not cloneable: it reports exactly one outcome.
pub struct Lease {
    store: Arc<CredentialStore>,
    pub credential: Arc<Credential>,
    pub selection: Selection,
    /// The model key the outcome is recorded under (Go `stateModelForExecution`).
    pub execution_model: String,
    pub attempt: u64,
    policy: Arc<Policy>,
    reported: bool,
    /// The scheduler generation at the pick (`Scheduler::reserve_probe`): bounded
    /// windows opened or probes reserved after it are not answered by this attempt.
    picked: u32,
    /// A credential from the remote dispatcher: its lease ends there, not in the local
    /// scheduler.
    remote: Option<crate::remote::RemoteEnd>,
    /// The LCP binding of the pick: the attempt's canonical session and lineage.
    pub lcp: Option<crate::lcp::Match>,
}

impl Lease {
    /// Whether the credential came from the remote dispatcher (Go Home mode).
    pub fn is_remote(&self) -> bool {
        self.remote.is_some()
    }

    /// Resolves when the dispatcher cancels this lease's execution.
    pub(crate) fn remote_cancelled(&self) -> Option<futures_util::future::BoxFuture<'static, ()>> {
        let signal = self.remote.as_ref()?.cancel.clone()?;
        Some(crate::remote::cancelled(signal))
    }

    /// Whether the dispatcher cancelled this lease's execution (Go: its selection is
    /// no longer `Active`).
    pub(crate) fn remote_cancel_requested(&self) -> bool {
        self.remote
            .as_ref()
            .and_then(|remote| remote.cancel.as_ref())
            .is_some_and(|signal| *signal.borrow())
    }

    pub fn complete(mut self, outcome: Outcome) {
        self.report(outcome);
    }

    /// Joins a remote lease's release to `releases`: a Home pick a session kept ends or
    /// runs again during a later request, which awaits the release before its next pick.
    pub(crate) fn release_into(&mut self, releases: &crate::remote::PendingReleases) {
        if let Some(remote) = &mut self.remote {
            remote.releases = releases.clone();
        }
    }

    /// The upstream accepted this attempt (a stream's first chunk arrived): an ended
    /// bounded window it probes has its answer, so other requests may use the account
    /// while the stream runs (`Scheduler::accept_probe`). A no-op for remote leases.
    pub fn accepted(&self) {
        if self.remote.is_none() {
            self.store.accept_probe(self);
        }
    }

    /// Records an intermediate outcome for one model of a pooled alias without ending
    /// the lease.
    pub fn note(&self, model: &str, outcome: &Outcome) {
        if self.remote.is_none() {
            self.store.record_model(self, model, outcome);
        }
    }

    fn report(&mut self, outcome: Outcome) {
        if std::mem::replace(&mut self.reported, true) {
            return;
        }
        match &mut self.remote {
            // Go `reportHomeResult` leaves local cooldowns alone; the scope ends with
            // its release.
            Some(remote) => remote.finish(),
            None => self.store.record(self, &outcome),
        }
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
    /// Bumped on every reconcile and patch, including pure removals.
    epoch: u64,
}

pub struct CredentialStore {
    inner: RwLock<Inner>,
    policy: RwLock<Arc<Policy>>,
    scheduler: Mutex<Scheduler>,
    attempts: AtomicU64,
    stats: [AtomicU64; 3],
    prepare_locks: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
    /// `auth-dir` while `save-cooldown-status` is on (Go `cooldownStore`).
    cooldown_dir: RwLock<Option<std::path::PathBuf>>,
    /// Replaces the `.cds` files while set (Go's token-store cooldown provider).
    cooldown_backend: RwLock<Option<Arc<dyn crate::cooldown_store::Backend>>>,
    /// Serializes cooldown snapshots with their writes, so the last write is the newest.
    cooldown_write: Mutex<()>,
    activity: Mutex<HashMap<String, CredentialActivity>>,
    /// Credential revisions whose file already holds Go's persisted form.
    persisted: Mutex<HashMap<String, u64>>,
    /// Where failed attempts publish Go's error events (set by the runtime).
    error_events: std::sync::OnceLock<Arc<crate::usage::UsageQueue>>,
    /// Marked on every epoch bump, so background loops sleep until the set changes.
    changed: tokio::sync::watch::Sender<()>,
}

impl CredentialStore {
    pub fn new(credentials: Vec<Credential>) -> Arc<Self> {
        let mut inner = Inner {
            creds: Vec::new(),
            generation: 0,
            epoch: 0,
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
            cooldown_dir: RwLock::default(),
            cooldown_backend: RwLock::default(),
            cooldown_write: Mutex::default(),
            activity: Mutex::default(),
            persisted: Mutex::default(),
            error_events: std::sync::OnceLock::new(),
            changed: tokio::sync::watch::Sender::new(()),
        })
    }

    /// Go `ApplyConfigWithCooldownStateStore` plus the restore that follows a config
    /// update: the current state goes to the old store before the swap, and a newly
    /// enabled store is restored from. `None` disables persistence.
    pub fn configure_cooldown_store(&self, dir: Option<std::path::PathBuf>) {
        let old = self.cooldown_dir.read().unwrap_or_else(PoisonError::into_inner).clone();
        if old == dir {
            return;
        }
        if old.is_some() {
            self.persist_cooldowns();
        }
        *self.cooldown_dir.write().unwrap_or_else(PoisonError::into_inner) = dir.clone();
        if let Some(dir) = dir {
            self.restore_cooldowns(&dir);
        }
    }

    /// Go's builder taking the token store's `CooldownStateStore`: cooldowns go to
    /// `backend` instead of `.cds` files, restored from it now when persistence is on.
    pub fn set_cooldown_backend(&self, backend: Arc<dyn crate::cooldown_store::Backend>) {
        *self.cooldown_backend.write().unwrap_or_else(PoisonError::into_inner) = Some(backend);
        let dir = self.cooldown_dir.read().unwrap_or_else(PoisonError::into_inner).clone();
        if let Some(dir) = dir {
            self.restore_cooldowns(&dir);
        }
    }

    fn cooldown_backend(&self) -> Option<Arc<dyn crate::cooldown_store::Backend>> {
        self.cooldown_backend
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Go `RestoreCooldownStates`: live records of live credentials whose cooling is
    /// enabled, then a rewrite that drops everything else.
    fn restore_cooldowns(&self, dir: &std::path::Path) {
        let loaded = match self.cooldown_backend() {
            Some(backend) => backend.load(),
            None => crate::cooldown_store::load(dir),
        };
        let records = match loaded {
            Ok(records) => records,
            Err(error) => {
                tracing::warn!(%error, "failed to restore cooldown state");
                return;
            }
        };
        if !records.is_empty() {
            let policy = self.policy.read().unwrap_or_else(PoisonError::into_inner).clone();
            let inner = self.read();
            let mut scheduler = self.scheduler.lock().unwrap_or_else(PoisonError::into_inner);
            let (now, wall) = (Instant::now(), std::time::SystemTime::now());
            for record in &records {
                let Some(c) = inner.creds.iter().find(|c| c.id == record.auth_id.trim()) else {
                    continue;
                };
                if c.disabled || policy.cooling_disabled(c) {
                    continue;
                }
                let has_models = records
                    .iter()
                    .any(|r| r.auth_id.trim() == c.id && !r.model.trim().is_empty());
                scheduler.restore(c, record, has_models, policy.max_trusted_cooldown, now, wall);
            }
        }
        self.persist_cooldowns();
    }

    /// Go `persistCooldownStates`: rewrites the `.cds` files from the live cooldowns.
    pub fn persist_cooldowns(&self) {
        let Some(dir) = self.cooldown_dir.read().unwrap_or_else(PoisonError::into_inner).clone() else {
            return;
        };
        let _write = self.cooldown_write.lock().unwrap_or_else(PoisonError::into_inner);
        let wall = std::time::SystemTime::now();
        let mut records = {
            let policy = self.policy.read().unwrap_or_else(PoisonError::into_inner).clone();
            let inner = self.read();
            let scheduler = self.scheduler.lock().unwrap_or_else(PoisonError::into_inner);
            let now = Instant::now();
            inner
                .creds
                .iter()
                .filter(|c| !c.disabled && !policy.cooling_disabled(c))
                .flat_map(|c| scheduler.records(c, now, wall))
                .collect::<Vec<_>>()
        };
        records.sort_by(|a, b| (&a.provider, &a.auth_id, &a.model).cmp(&(&b.provider, &b.auth_id, &b.model)));
        let saved = match self.cooldown_backend() {
            Some(backend) => backend.save(records, wall),
            None => crate::cooldown_store::save(&dir, records, wall),
        };
        if let Err(error) = saved {
            tracing::warn!(%error, "failed to persist cooldown state");
        }
    }

    /// Go `Manager.persist` after a result (`FileTokenStore.Save`, metadata branch): the
    /// file gets `"disabled"` and Go's `json.Marshal` form unless it already holds the
    /// same JSON. Checked once per revision, so a file is not read on every request.
    // ponytail: atomic replace instead of Go's in-place truncate, and a removed file is
    // not recreated (Go recreates one deleted between watcher reloads).
    fn persist_credential(&self, credential: &Arc<Credential>) {
        let Source::File(path) = &credential.source else {
            return;
        };
        if credential.disabled
            || credential
                .attributes
                .get("runtime_only")
                .is_some_and(|v| v.trim().eq_ignore_ascii_case("true"))
        {
            return;
        }
        {
            let persisted = self.persisted.lock().unwrap_or_else(PoisonError::into_inner);
            if persisted.get(&credential.id) == Some(&credential.revision) {
                return;
            }
        }
        // The store write lock serializes this with `apply_patch`: a refresh committed
        // after this request started must not be overwritten with the older token.
        let mut inner = self.inner.write().unwrap_or_else(PoisonError::into_inner);
        let Some(slot) = inner
            .creds
            .iter_mut()
            .find(|c| c.id == credential.id && c.revision == credential.revision)
        else {
            return;
        };
        let mut metadata = slot.metadata.clone();
        metadata.insert("disabled".into(), Value::Bool(slot.disabled));
        let go = |bytes: &[u8]| cpa_common::json::GoValue::parse_f64(bytes);
        let Some(target) = serde_json::to_vec(&metadata).ok().and_then(|b| go(&b)) else {
            return;
        };
        let Ok(existing) = std::fs::read(path) else {
            return;
        };
        if go(&existing).as_ref() != Some(&target) {
            let bytes = target.marshal();
            if let Err(error) = write_bytes_atomic(path, &bytes) {
                tracing::warn!(id = %credential.id, %error, "failed to persist credential");
                return;
            }
            // Same revision: the file now says what memory already meant (absent
            // `disabled` is false, numbers are float64), so memory takes the written
            // form and the watcher's reload finds nothing changed.
            if let Ok(written) = serde_json::from_slice::<Map<String, Value>>(&bytes) {
                let mut next = Credential::clone(slot);
                next.metadata = written;
                *slot = Arc::new(next);
            }
        }
        drop(inner);
        self.persisted
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(credential.id.clone(), credential.revision);
    }

    /// Go `clearDisabledCooldownStates` after a policy change.
    fn clear_disabled_cooldowns(&self) {
        let policy = self.policy.read().unwrap_or_else(PoisonError::into_inner).clone();
        let cleared = {
            let inner = self.read();
            self.scheduler
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .clear_disabled(&inner.creds, &policy)
        };
        if cleared {
            self.persist_cooldowns();
        }
    }

    pub fn snapshot(&self) -> Vec<Arc<Credential>> {
        self.read().creds.clone()
    }

    pub fn get(&self, id: &str) -> Option<Arc<Credential>> {
        self.read().creds.iter().find(|c| c.id == id).cloned()
    }

    /// Selects among the selection's providers without registry admission (tests and
    /// callers that already filtered credentials).
    pub fn select(self: &Arc<Self>, selection: Selection) -> Option<Lease> {
        let policy = self.policy.read().unwrap_or_else(PoisonError::into_inner).clone();
        let scope = selection.clone();
        let admit = standalone_admission(&scope, &policy);
        self.select_with(selection, policy.clone(), &admit).ok()
    }

    pub fn select_with(
        self: &Arc<Self>,
        selection: Selection,
        policy: Arc<Policy>,
        admit: &Admit<'_>,
    ) -> Result<Lease, AcquireError> {
        self.select_ranked(selection, policy, admit, &|_| crate::scheduler::Windows::default())
    }

    /// [`Self::select_with`] with each credential's usage windows (`soonest-reset`).
    pub fn select_ranked(
        self: &Arc<Self>,
        selection: Selection,
        policy: Arc<Policy>,
        admit: &Admit<'_>,
        ranks: &crate::scheduler::Ranks<'_>,
    ) -> Result<Lease, AcquireError> {
        let now = Instant::now();
        let credential = {
            let inner = self.read();
            let mut scheduler = self.scheduler.lock().unwrap_or_else(PoisonError::into_inner);
            let eligible: Vec<_> = inner
                .creds
                .iter()
                .filter(|c| !c.disabled && !selection.exclude.contains(&c.id))
                .filter(|c| policy.retry_limit(c) >= selection.retry_round && scheduler.admits(c, &policy))
                .filter_map(|c| admit(c).map(|m| (c, m)))
                .collect();
            let candidates: Vec<_> = eligible
                .iter()
                .filter(|(c, m)| scheduler.wait(c, m, now).is_none())
                .map(|(c, _)| (c.as_ref(), crate::registry::provider_key(c)))
                .collect();
            if candidates.is_empty() {
                let cause = eligible
                    .iter()
                    .filter_map(|(c, m)| scheduler.last_error(c, m))
                    .max_by_key(|s| s.deadline)
                    .map(|s| s.error.clone());
                let wait = eligible.iter().filter_map(|(c, m)| scheduler.wait(c, m, now)).min();
                if !eligible.is_empty() && eligible.iter().all(|(c, m)| scheduler.quota_cooling(c, m, now)) {
                    return Err(AcquireError::Cooldown {
                        wait: wait.unwrap_or_default(),
                        cause,
                    });
                }
                return Err(AcquireError::Unavailable {
                    retry_after: if eligible.is_empty() { None } else { wait },
                    cause,
                });
            }
            let refs: Vec<(&Credential, &str)> = candidates.iter().map(|(c, p)| (*c, p.as_str())).collect();
            let (picked, lcp) = scheduler.pick_session(&refs, &selection, &policy, ranks, now).unwrap();
            let (c, m) = eligible.iter().find(|(c, _)| c.id == picked.id).unwrap();
            let generation = scheduler.reserve_probe(c, m, now);
            (Arc::clone(c), lcp, generation)
        };
        let (credential, lcp, picked) = credential;
        let execution_model = admit(&credential).unwrap_or_else(|| selection.model.clone());
        Ok(Lease {
            store: self.clone(),
            credential,
            selection,
            execution_model,
            attempt: self.attempts.fetch_add(1, Ordering::Relaxed),
            policy,
            reported: false,
            picked,
            remote: None,
            lcp,
        })
    }

    /// The registry projection of this credential's cooldown state for `model`.
    pub fn suspension(&self, credential: &Credential, model: &str) -> crate::registry::Suspension {
        use crate::registry::Suspension;
        let scheduler = self.scheduler.lock().unwrap_or_else(PoisonError::into_inner);
        let now = Instant::now();
        let live = |m: &str| {
            scheduler
                .cooldowns
                .get(&(credential.id.clone(), crate::scheduler::canonical_model(m).to_owned()))
                .filter(|s| s.deadline > now)
        };
        if let Some(state) = live(model) {
            return match (state.quota, state.status) {
                (true, 429) => Suspension::Quota,
                (quota_exceeded, _) => Suspension::Other { quota_exceeded },
            };
        }
        if live("").is_some() {
            // Credential-wide quota: Go reports these models as `credential_quota`.
            return Suspension::Other { quota_exceeded: false };
        }
        Suspension::None
    }

    /// Whether `model` is cooling for this credential right now. A probe reservation does
    /// not count: the lease that holds it is the one asking.
    pub fn blocked(&self, credential: &Credential, model: &str) -> bool {
        let scheduler = self.scheduler.lock().unwrap_or_else(PoisonError::into_inner);
        scheduler.cooling(credential, model, Instant::now())
    }

    /// Clears every cooldown and affinity binding of one credential (Go `ResetQuota`).
    /// Returns the model keys that were cooling.
    pub fn reset_cooldown(&self, id: &str) -> Vec<String> {
        let models = self.scheduler.lock().unwrap_or_else(PoisonError::into_inner).reset(id);
        if !models.is_empty() {
            self.persist_cooldowns();
        }
        models
    }

    /// Returns the next round's wait, if any credential still permits that round
    /// (Go `closestCooldownWaitWithAttempted`). A wait exceeding the cap is rejected,
    /// not shortened. `status` is the round error's status; `attempted` the credentials
    /// that executed this round (they get the 10s quota floor after a 429).
    pub fn retry_wait(
        &self,
        selection: &Selection,
        policy: &Policy,
        status: u16,
        attempted: &[String],
        admit: &Admit<'_>,
    ) -> Option<Duration> {
        self.retry_wait_at(selection, policy, status, attempted, admit, Instant::now())
    }

    fn retry_wait_at(
        &self,
        selection: &Selection,
        policy: &Policy,
        status: u16,
        attempted: &[String],
        admit: &Admit<'_>,
        now: Instant,
    ) -> Option<Duration> {
        let inner = self.read();
        let scheduler = self.scheduler.lock().unwrap_or_else(PoisonError::into_inner);
        let wait = inner
            .creds
            .iter()
            .filter(|c| !c.disabled)
            .filter(|c| policy.retry_limit(c) > selection.retry_round && scheduler.admits(c, policy))
            .filter_map(|c| admit(c).map(|m| (c, m)))
            .filter(|(c, m)| scheduler.retry_eligible(c, m, now))
            .map(|(c, m)| {
                let wait = scheduler.wait(c, &m, now).unwrap_or_default();
                if status == 429 && attempted.contains(&c.id) && !policy.cooling_disabled(c) {
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

    fn accept_probe(&self, lease: &Lease) {
        let inner = self.read();
        // Like results: a credential edited or re-created since the pick is left alone.
        if !inner
            .creds
            .iter()
            .any(|c| c.id == lease.credential.id && c.revision == lease.credential.revision)
        {
            return;
        }
        self.scheduler
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .accept_probe_picked(&lease.credential, &lease.execution_model, lease.picked, Instant::now());
    }

    fn record(&self, lease: &Lease, outcome: &Outcome) {
        self.record_model(lease, &lease.execution_model, outcome);
    }

    fn record_model(&self, lease: &Lease, model: &str, outcome: &Outcome) {
        let slot = match outcome {
            Outcome::Success => 0,
            Outcome::Failure(_) | Outcome::Neutral(_) => 1,
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
            let (now, wall) = (Instant::now(), std::time::SystemTime::now());
            let persist = self
                .cooldown_dir
                .read()
                .unwrap_or_else(PoisonError::into_inner)
                .is_some();
            let mut scheduler = self.scheduler.lock().unwrap_or_else(PoisonError::into_inner);
            let before = persist.then(|| scheduler.records(&lease.credential, now, wall));
            scheduler.record_picked(&lease.credential, model, outcome, &lease.policy, lease.picked, now);
            scheduler.session_result(
                &lease.credential,
                &lease.selection,
                outcome,
                &lease.policy,
                lease.lcp.as_ref(),
                now,
            );
            // Go `publishErrorEvent` after MarkResult (and the availability-neutral record).
            if let (Outcome::Failure(error) | Outcome::Neutral(error), Some(queue)) = (outcome, self.error_events.get())
                && queue.wants_errors()
            {
                let records = scheduler.records(&lease.credential, now, wall);
                queue.enqueue_error(&crate::error_events::payload(&lease.credential, model, error, &records));
            }
            // Go MarkResult persists only when this credential's cooldown records changed.
            let changed = before.is_some_and(|before| before != scheduler.records(&lease.credential, now, wall));
            drop(scheduler);
            drop(inner);
            if changed {
                self.persist_cooldowns();
            }
            // Go persists on every recorded result, a client-cancelled stream included.
            self.persist_credential(&lease.credential);
        }
    }

    /// Resolves after the next change to the credential set or any credential.
    pub fn subscribe(&self) -> tokio::sync::watch::Receiver<()> {
        self.changed.subscribe()
    }

    /// Changes whenever the credential set or any credential changes.
    pub fn epoch(&self) -> u64 {
        self.read().epoch
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
        // Go `FileTokenStore.Save` always records the disabled state.
        next.metadata.insert("disabled".into(), Value::Bool(next.disabled));
        write_atomic(path, &next.metadata).map_err(|e| PatchError::Io(e.to_string()))?;
        *slot = Arc::new(next);
        let committed = slot.clone();
        inner.generation = generation;
        inner.epoch += 1;
        self.changed.send_replace(());
        Ok(committed)
    }

    /// Replaces a config-backed or runtime-only credential in memory if it is still at
    /// `expected_revision`: Go's `Manager.Update` never persists config API keys (the
    /// next config publish re-synthesizes them) or `runtime_only` auths. `NotFound`
    /// unless `next.id` names such a credential.
    pub fn replace_config_backed(
        &self,
        next: Credential,
        expected_revision: u64,
    ) -> Result<Arc<Credential>, PatchError> {
        let mut inner = self.inner.write().unwrap_or_else(PoisonError::into_inner);
        let generation = inner.generation + 1;
        let slot = inner
            .creds
            .iter_mut()
            .find(|c| c.id == next.id && matches!(c.source, Source::Config { .. } | Source::Runtime))
            .ok_or(PatchError::NotFound)?;
        if slot.revision != expected_revision {
            return Err(PatchError::Stale { current: slot.revision });
        }
        let mut next = next;
        next.source = slot.source.clone();
        next.revision = generation;
        *slot = Arc::new(next);
        let committed = slot.clone();
        inner.generation = generation;
        // Registrations depend on the credential (prefix, models): invalidate the
        // registry cache like `apply_patch` does.
        inner.epoch += 1;
        self.changed.send_replace(());
        drop(inner);
        // Go `Manager.Update` clears the cooldowns of an auth it stores disabled (or
        // with cooling off), so a later re-enable routes to it at once.
        self.clear_disabled_cooldowns();
        Ok(committed)
    }

    /// Replaces the credential set (watcher reload, management import/delete). Unchanged
    /// credentials keep their revision; new and changed ones get fresh revisions.
    pub fn reconcile(&self, credentials: Vec<Credential>) {
        let mut inner = self.inner.write().unwrap_or_else(PoisonError::into_inner);
        let mut next = Vec::with_capacity(credentials.len());
        for mut cred in credentials {
            match inner.creds.iter().find(|c| unchanged(c, &cred)) {
                Some(existing) => next.push(existing.clone()),
                None => {
                    inner.generation += 1;
                    cred.revision = inner.generation;
                    next.push(Arc::new(cred));
                }
            }
        }
        // Runtime-only credentials (relay sessions) have no file or config entry; they
        // stay until their session ends.
        let runtime: Vec<_> = inner
            .creds
            .iter()
            .filter(|c| c.source == Source::Runtime && !next.iter().any(|n| n.id == c.id))
            .cloned()
            .collect();
        next.extend(runtime);
        inner.creds = next;
        inner.epoch += 1;
        self.changed.send_replace(());
        self.scheduler
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .reconcile(&inner.creds);
        drop(inner);
        self.clear_disabled_cooldowns();
        self.persist_cooldowns();
    }

    /// Inserts or replaces one credential (a plugin's `host.auth.save`) under a single
    /// write lock, so a concurrent `apply_patch` on another credential is never undone
    /// the way a snapshot followed by [`Self::reconcile`] could undo it. An unchanged
    /// credential keeps its revision, as in `reconcile`.
    pub fn upsert(&self, mut credential: Credential) {
        let mut inner = self.inner.write().unwrap_or_else(PoisonError::into_inner);
        let slot = inner.creds.iter().position(|c| c.id == credential.id);
        if slot.is_some_and(|i| unchanged(&inner.creds[i], &credential)) {
            return;
        }
        inner.generation += 1;
        credential.revision = inner.generation;
        let credential = Arc::new(credential);
        match slot {
            Some(i) => inner.creds[i] = credential,
            None => inner.creds.push(credential),
        }
        inner.epoch += 1;
        self.changed.send_replace(());
        self.scheduler
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .reconcile(&inner.creds);
        drop(inner);
        self.clear_disabled_cooldowns();
        self.persist_cooldowns();
    }

    /// Adds a runtime-only credential (Go's auth add for a `/v1/ws` relay session)
    /// unless one with its ID is already active. Additive API for the relay route.
    pub fn add_runtime(&self, mut credential: Credential) -> bool {
        let mut inner = self.inner.write().unwrap_or_else(PoisonError::into_inner);
        if inner.creds.iter().any(|c| c.id == credential.id && !c.disabled) {
            return false;
        }
        inner.creds.retain(|c| c.id != credential.id);
        inner.generation += 1;
        credential.revision = inner.generation;
        inner.creds.push(Arc::new(credential));
        inner.epoch += 1;
        self.changed.send_replace(());
        self.scheduler
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .reconcile(&inner.creds);
        true
    }

    /// Removes a credential when its relay session ends (Go's auth delete). Additive
    /// API for the relay route.
    pub fn remove_runtime(&self, id: &str) {
        let mut inner = self.inner.write().unwrap_or_else(PoisonError::into_inner);
        let before = inner.creds.len();
        inner.creds.retain(|c| !(c.id == id && c.source == Source::Runtime));
        if inner.creds.len() == before {
            return;
        }
        inner.epoch += 1;
        self.changed.send_replace(());
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
    /// Credentials bound to live session-affinity keys that `matches`, without
    /// refreshing them (the plugin host's `host.affinity.lookup`).
    pub(crate) fn affinity_bound(&self, matches: impl Fn(&crate::affinity::Key) -> bool) -> Vec<String> {
        self.scheduler
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .affinity_bound(Instant::now(), matches)
    }

    pub fn cooldowns(&self, id: &str) -> Vec<crate::scheduler::CooldownState> {
        self.scheduler
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .cooldowns_of(id, Instant::now())
    }

    /// Clears the cooldowns of one credential (Go `Manager.ResetQuota`); returns the
    /// model keys that were cooling.
    pub fn reset_cooldowns(&self, id: &str) -> Vec<String> {
        let had = !self.cooldowns(id).is_empty();
        let models = self
            .scheduler
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .reset_cooldowns(id);
        if had {
            // Go `ResetQuota` persists the cleared state.
            self.persist_cooldowns();
        }
        models
    }
}

impl Runtime {
    /// Runs the executor's single-flighted preparation now, due or not (management
    /// refresh, Go `RefreshAuthFile`). Returns the committed credential.
    pub async fn refresh_credential(&self, id: &str) -> Result<Arc<Credential>, ExecError> {
        let cfg = self.config();
        let revision = self.store.get(id).map(|c| c.revision);
        self.prepare_credential(id, &cfg, revision).await
    }
}

/// Writes `metadata` the way Go's `json.NewEncoder(f).Encode` does (compact JSON and a
/// newline) via an exclusively created, uniquely named 0600 sibling and a rename. The
/// temp name does not end in `.json`, so a crash never leaves a loadable credential.
fn write_atomic(path: &Path, metadata: &Map<String, Value>) -> std::io::Result<()> {
    let mut bytes = serde_json::to_vec(metadata)?;
    bytes.push(b'\n');
    write_bytes_atomic(path, &bytes)
}

/// Replaces `path` with `bytes` through an exclusively created 0600 sibling.
pub(crate) fn write_bytes_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let dir = path.parent().unwrap_or(Path::new("."));
    let name = path.file_name().unwrap_or_default().to_string_lossy();
    let (tmp, mut file) = loop {
        let tmp = dir.join(format!(
            ".{name}.{}.{}.tmp",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        // create_new is O_CREAT|O_EXCL: it never follows or reuses an existing path.
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        // Go's 0600; on Windows Go ignores mode bits and creates the file plainly.
        #[cfg(unix)]
        std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
        match options.open(&tmp) {
            Ok(file) => break (tmp, file),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        }
    };
    let result = file
        .write_all(bytes)
        .and_then(|()| file.sync_all())
        .and_then(|()| std::fs::rename(&tmp, path));
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

/// A response stream that reports its lease's outcome on end, on the first error (after
/// which it yields nothing more), or as `Cancelled` when dropped early.
///
/// A remote lease's cancellation (Go's selection-bound attempt context) is watched by
/// its own task: a draining dispatcher closes the upstream body and ends the lease even
/// while the client is not reading, and the reader then sees `context canceled`.
pub struct Completing {
    slot: Arc<Mutex<Slot>>,
    watcher: Option<tokio::task::JoinHandle<()>>,
}

struct Slot {
    inner: ExecStream,
    lease: Option<Lease>,
    /// The watcher ended the lease; the next poll yields the cancellation error.
    cancelled: bool,
    waker: Option<std::task::Waker>,
}

impl Completing {
    pub fn new(inner: ExecStream, lease: Lease) -> Self {
        let cancelled = lease.remote_cancelled();
        let slot = Arc::new(Mutex::new(Slot {
            inner,
            lease: Some(lease),
            cancelled: false,
            waker: None,
        }));
        let watcher = cancelled.map(|cancelled| {
            let slot = Arc::downgrade(&slot);
            tokio::spawn(async move {
                cancelled.await;
                let Some(slot) = slot.upgrade() else { return };
                let (inner, lease, waker) = {
                    let mut s = slot.lock().unwrap_or_else(PoisonError::into_inner);
                    let Some(lease) = s.lease.take() else { return };
                    s.cancelled = true;
                    let inner = std::mem::replace(&mut s.inner, Box::pin(futures_util::stream::empty()));
                    (inner, lease, s.waker.take())
                };
                // The upstream body closes first, then the lease ends.
                drop(inner);
                lease.complete(Outcome::Cancelled);
                if let Some(waker) = waker {
                    waker.wake();
                }
            })
        });
        Self { slot, watcher }
    }
}

impl Drop for Completing {
    fn drop(&mut self) {
        if let Some(watcher) = &self.watcher {
            watcher.abort();
        }
    }
}

impl Stream for Completing {
    type Item = Result<Bytes, ExecError>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let mut s = self.slot.lock().unwrap_or_else(PoisonError::into_inner);
        if std::mem::take(&mut s.cancelled) {
            return Poll::Ready(Some(Err(crate::remote::cancelled_error())));
        }
        if s.lease.is_none() {
            return Poll::Ready(None);
        }
        if self.watcher.is_some() {
            s.waker = Some(cx.waker().clone());
        }
        let item = ready!(s.inner.poll_next_unpin(cx));
        match &item {
            Some(Ok(_)) => {}
            None => s.lease.take().unwrap().complete(Outcome::Success),
            Some(Err(e)) => {
                // The upstream body closes before the lease ends: a remote lease's release
                // must not reach the control plane while the response is still open.
                s.inner = Box::pin(futures_util::stream::empty());
                s.lease.take().unwrap().complete(Outcome::Failure(e.clone()));
            }
        }
        Poll::Ready(item)
    }
}

/// Go `configuredSignatureCacheEnabled` / `configuredSignatureBypassStrict`
/// (internal/api/server_reload.go): `oauth.providers.antigravity.signature-cache-enabled`
/// (default true) and `.signature-bypass-strict` (default false), legacy
/// `antigravity-signature-*` spellings included. Applied on every publish, as Go
/// applies them at startup and on each reload.
fn signature_cache_config(cfg: &Config) -> (bool, bool) {
    let flag = |key: &str| {
        cfg.document
            .get("oauth")
            .and_then(|o| o.get("providers"))
            .and_then(|p| p.get("antigravity"))
            .and_then(|a| a.get(key))
            .and_then(serde_yaml_ng::Value::as_bool)
    };
    (
        flag("signature-cache-enabled").unwrap_or(true),
        flag("signature-bypass-strict").unwrap_or(false),
    )
}

/// Credentials the background refresh considers: enabled auth files, never API-key
/// kinds (Go `nextRefreshCheckAt` skips `AuthKind() == apikey` for every provider).
fn refresh_candidate(c: &Credential) -> bool {
    matches!(c.source, Source::File(_)) && !c.disabled && cpa_core::registry::dynamic::auth_kind(c) != Some("apikey")
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;

    /// Go `nextRefreshCheckAt` skips `AuthKind() == apikey` for every provider. The
    /// kinds are Go's `AuthKind` goldens (tests/fixtures/server_go.json `auth_kind`).
    #[test]
    fn refresh_skips_api_key_kinds_like_go() {
        let fixture: serde_json::Value =
            serde_json::from_str(include_str!("../tests/fixtures/server_go.json")).unwrap();
        let mut kinds = std::collections::BTreeSet::new();
        for case in fixture["auth_kind"].as_array().unwrap() {
            let meta = serde_json::json!({"type": "kimi"}).as_object().unwrap().clone();
            let mut c = Credential::from_file(Path::new("/a"), Path::new("/a/x.json"), meta).unwrap();
            c.metadata = case["metadata"].as_object().cloned().unwrap_or_default();
            c.attributes = case["attributes"]
                .as_object()
                .map(|a| {
                    a.iter()
                        .map(|(k, v)| (k.clone(), v.as_str().unwrap().to_owned()))
                        .collect()
                })
                .unwrap_or_default();
            let kind = case["kind"].as_str().unwrap();
            kinds.insert(kind);
            assert_eq!(refresh_candidate(&c), kind != "apikey", "case {case}");
            c.disabled = true;
            assert!(!refresh_candidate(&c));
        }
        assert!(kinds.contains("apikey") && kinds.contains("oauth"), "{kinds:?}");
    }

    #[test]
    fn signature_cache_config_reads_v8_and_legacy_keys() {
        let at = |yaml: &str| signature_cache_config(&Config::parse(yaml).unwrap());
        assert_eq!(at("{}\n"), (true, false));
        assert_eq!(
            at("antigravity-signature-cache-enabled: false\nantigravity-signature-bypass-strict: true\n"),
            (false, true)
        );
        assert_eq!(
            at("oauth: {providers: {antigravity: {signature-cache-enabled: false, signature-bypass-strict: true}}}\n"),
            (false, true)
        );
        assert_eq!(
            at("oauth: {providers: {antigravity: {signature-cache-enabled: null}}}\n"),
            (true, false)
        );
    }

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
    fn standalone_admission_reads_attributes_first() {
        let mut c = cred("a.json", "claude", false);
        c.metadata.insert("prefix".into(), "meta".into());
        c.metadata
            .insert("excluded_models".into(), serde_json::json!(["claude-sonnet*"]));
        c.attributes.insert("prefix".into(), "team".into());
        c.attributes.insert("excluded_models".into(), "claude-opus*".into());
        let policy = Policy::default();
        let admit = |model: &str| {
            let s = Selection::new("claude", model);
            standalone_admission(&s, &policy)(&c)
        };
        assert_eq!(
            admit("team/claude-sonnet-5(high)").as_deref(),
            Some("claude-sonnet-5(high)")
        );
        assert_eq!(admit("team/claude-opus-5"), None, "attribute exclusions win");
        assert_eq!(
            admit("meta/claude-sonnet-5"),
            None,
            "metadata prefix is not authoritative"
        );
        assert_eq!(admit("claude-sonnet-5").as_deref(), Some("claude-sonnet-5"));
        let forced = Policy {
            force_model_prefix: true,
            ..Policy::default()
        };
        let s = Selection::new("claude", "claude-sonnet-5");
        assert_eq!(standalone_admission(&s, &forced)(&c), None);
        assert_eq!(standalone_admission(&Selection::new("codex", "x"), &policy)(&c), None);
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
        #[cfg(unix)]
        std::os::unix::fs::symlink(
            &victim,
            dir.join(format!(".claude-a.json.{}.0.tmp", std::process::id())),
        )
        .unwrap();
        let mut cfg = cpa_core::config::Config::parse("").unwrap();
        cfg.auth_dir = dir.clone();
        let store = CredentialStore::new(cpa_core::config::credentials::from_auth_dir(&cfg));
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
        #[cfg(unix)]
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

    /// A plugin save running alongside token refreshes of another credential never
    /// reinstalls a stale copy of it: every refresh finds the revision it left.
    #[test]
    fn upsert_never_undoes_a_concurrent_patch() {
        let dir = std::env::temp_dir().join(format!("cpa-upsert-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("refreshed.json"), r#"{"type":"claude","refresh_token":"r0"}"#).unwrap();
        std::fs::write(dir.join("saved.json"), r#"{"type":"claude"}"#).unwrap();
        let mut cfg = cpa_core::config::Config::parse("").unwrap();
        cfg.auth_dir = dir.clone();
        let store = Arc::new(CredentialStore::new(cpa_core::config::credentials::from_auth_dir(&cfg)));
        let saved = (*store.get("saved.json").unwrap()).clone();
        let saver = {
            let store = store.clone();
            std::thread::spawn(move || {
                for n in 0..500 {
                    let mut next = saved.clone();
                    next.metadata.insert("n".into(), n.into());
                    store.upsert(next);
                }
            })
        };
        let mut revision = store.get("refreshed.json").unwrap().revision;
        for n in 1..=500 {
            let mut patch = MetadataPatch::default();
            patch.set.insert("refresh_token".into(), format!("r{n}").into());
            revision = store
                .apply_patch("refreshed.json", revision, &patch)
                .unwrap_or_else(|e| panic!("refresh {n} was undone: {e:?}"))
                .revision;
        }
        saver.join().unwrap();
        let refreshed = store.get("refreshed.json").unwrap();
        assert_eq!(refreshed.revision, revision);
        assert_eq!(refreshed.metadata["refresh_token"], "r500");
        assert_eq!(store.get("saved.json").unwrap().metadata["n"], 499);
        assert_eq!(store.snapshot().len(), 2);
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

    /// Go `TestRetryIntervalFiltersCooldownCredentials` and
    /// `TestRetryRoundAvailabilityRejectsStaleQuotaForNonRetryableStatus`: the next round
    /// waits for the earliest cooldown within max-retry-interval, skipping longer ones,
    /// and a credential cooling after a 402 or 404 never permits a round, whatever its
    /// deadline, while one cooling after a 429 does.
    #[test]
    fn retry_wait_skips_cooldowns_beyond_the_cap_and_non_retryable_statuses() {
        let now = Instant::now();
        let selection = sel("claude");
        let admit_all = |_: &Credential| Some(String::new());
        // One credential per (status, retry-after seconds), each cooling from `now`.
        let wait = |cap: u64, cooldowns: &[(u16, u64)]| {
            let policy = Policy {
                request_retry: 1,
                max_retry_interval: Duration::from_secs(cap),
                ..Policy::default()
            };
            let ids: Vec<String> = (0..cooldowns.len()).map(|i| format!("c{i}.json")).collect();
            let store = CredentialStore::new(ids.iter().map(|id| cred(id, "claude", false)).collect());
            for (id, (status, seconds)) in ids.iter().zip(cooldowns) {
                let mut error = ExecError::local(*status, FailureScope::Model, "cooling");
                error.retry_after = Some(Duration::from_secs(*seconds));
                let credential = store.get(id).unwrap();
                store.scheduler.lock().unwrap().record(
                    &credential,
                    &selection.model,
                    &Outcome::Failure(error),
                    &policy,
                    now,
                );
            }
            store.retry_wait_at(&selection, &policy, 429, &[], &admit_all, now)
        };
        let short_and_long = wait(30, &[(429, 10), (429, 60)]);
        assert!(
            short_and_long.is_some_and(|w| !w.is_zero() && w <= Duration::from_secs(10)),
            "{short_and_long:?}"
        );
        assert_eq!(wait(30, &[(429, 60)]), None, "only a cooldown beyond the cap");
        assert!(wait(3600, &[(429, 60)]).is_some(), "rate limit");
        assert_eq!(wait(3600, &[(402, 60)]), None, "payment required");
        assert_eq!(wait(3600, &[(404, 60)]), None, "not found");
    }

    #[test]
    fn retry_wait_obeys_exact_cap_and_attempted_quota_floor() {
        let store = CredentialStore::new(vec![cred("a.json", "claude", false)]);
        let now = Instant::now();
        let mut selection = sel("claude");
        let admit_all = |_: &Credential| Some(String::new());
        let tried = ["a.json".to_owned()];
        let mut policy = Policy {
            request_retry: 1,
            ..Policy::default()
        };
        // A transport fault (status 0) retries immediately.
        assert_eq!(
            store.retry_wait_at(&selection, &policy, 0, &tried, &admit_all, now),
            Some(Duration::ZERO)
        );
        // An attempted credential after a 429 waits at least 10s, which the default
        // max-retry-interval of 0 forbids; an untried one may go immediately.
        assert_eq!(
            store.retry_wait_at(&selection, &policy, 429, &tried, &admit_all, now),
            None
        );
        assert_eq!(
            store.retry_wait_at(&selection, &policy, 429, &[], &admit_all, now),
            Some(Duration::ZERO)
        );
        policy.max_retry_interval = Duration::from_secs(10);
        assert_eq!(
            store.retry_wait_at(&selection, &policy, 429, &tried, &admit_all, now),
            Some(Duration::from_secs(10))
        );
        let quota = ExecError::local(429, FailureScope::Model, "quota");
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
            store.retry_wait_at(&selection, &policy, 429, &tried, &admit_all, later),
            Some(Duration::from_secs(10))
        );
        policy.max_retry_interval = Duration::from_secs(9);
        assert_eq!(
            store.retry_wait_at(&selection, &policy, 429, &tried, &admit_all, later),
            None
        );
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
            store.retry_wait_at(&selection, &policy, 429, &tried, &admit_all, later),
            Some(Duration::ZERO),
            "disabled cooling has no floor"
        );
        selection.retry_round = 1;
        assert_eq!(
            store.retry_wait_at(&selection, &policy, 0, &tried, &admit_all, later),
            None
        );
    }

    /// A 429 with a reset six days away (`max-trusted-cooldown` bounds it).
    fn weekly_limit(scope: FailureScope) -> Outcome {
        let mut error = ExecError::local(429, scope, "usage_limit_reached");
        error.retry_after = Some(Duration::from_secs(6 * 24 * 3600));
        Outcome::Failure(error)
    }

    /// A probe whose hold lapsed was taken over by a later pick: the first probe's
    /// cancellation leaves the account reserved for the second, whose own does not.
    #[test]
    fn a_stale_probe_cancellation_keeps_the_newer_reservation() {
        let store = CredentialStore::new(vec![cred("a.json", "claude", false)]);
        let selection = Selection::new("claude", "m");
        let credential = store.get("a.json").unwrap();
        // A bounded window of the 10 s minimum that ended a second ago.
        let policy = Policy {
            max_trusted_cooldown: Duration::from_secs(10),
            ..Policy::default()
        };
        let opened = Instant::now().checked_sub(Duration::from_secs(11)).unwrap();
        store
            .scheduler
            .lock()
            .unwrap()
            .record(&credential, "m", &weekly_limit(FailureScope::Model), &policy, opened);
        let first = store.select(selection.clone()).expect("the probe");
        assert_eq!(first.execution_model, "m");
        assert!(store.select(selection.clone()).is_none(), "reserved");
        // The first probe's hold lapses, and the next pick probes again.
        let key = ("a.json".to_owned(), "m".to_owned());
        store
            .scheduler
            .lock()
            .unwrap()
            .cooldowns
            .get_mut(&key)
            .unwrap()
            .trust
            .probe = Some(Instant::now());
        let second = store.select(selection.clone()).expect("the second probe");
        first.complete(Outcome::Cancelled);
        assert!(store.select(selection.clone()).is_none(), "still reserved");
        second.complete(Outcome::Cancelled);
        assert!(store.select(selection).is_some(), "its own cancellation frees it");
    }

    /// A request picked before a bounded window opened succeeds after it (a stream that
    /// outlived the 429): its answer is older than the window, which stays, live or
    /// ended, until its own probe.
    #[test]
    fn a_late_success_keeps_a_newer_window() {
        let selection = Selection::new("claude", "m");
        // A model window, opened by a request picked after the stream.
        let store = CredentialStore::new(vec![cred("a.json", "claude", false)]);
        let stream = store.select(selection.clone()).unwrap();
        let other = store.select(selection.clone()).unwrap();
        other.complete(weekly_limit(FailureScope::Model));
        assert!(store.select(selection.clone()).is_none(), "cooling");
        stream.accepted();
        stream.complete(Outcome::Success);
        assert!(store.select(selection.clone()).is_none(), "the window stands");
        // A credential-wide window that already ended keeps its probe.
        let store = CredentialStore::new(vec![cred("a.json", "claude", false)]);
        let stream = store.select(selection.clone()).unwrap();
        let policy = Policy {
            max_trusted_cooldown: Duration::from_secs(10),
            ..Policy::default()
        };
        let opened = Instant::now().checked_sub(Duration::from_secs(11)).unwrap();
        store.scheduler.lock().unwrap().record(
            &store.get("a.json").unwrap(),
            "m",
            &weekly_limit(FailureScope::Credential),
            &policy,
            opened,
        );
        stream.accepted();
        stream.complete(Outcome::Success);
        let _probe = store.select(selection.clone()).expect("the window's probe");
        assert!(store.select(selection).is_none(), "one probe");
    }

    #[test]
    fn config_routing_drives_policy_at_startup_and_on_publish() {
        let executors = || Executors {
            claude: cpa_exec::claude::ClaudeExecutor::new("http://127.0.0.1:1").unwrap(),
            codex: Default::default(),
            devices: Default::default(),
            openai: Default::default(),
            google: Default::default(),
        };
        // Legacy top-level keys and the canonical routing block both reach the scheduler.
        let rt = crate::testing::runtime(
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

    /// Go `nextModelPoolOffset` (conductor_models.go): per-key cursor, no advance for
    /// single-entry pools or blank keys, reset to zero at the int32 guard.
    #[test]
    fn pool_offsets_rotate_per_key_like_go() {
        let rt = crate::testing::runtime(
            Config::parse("").unwrap(),
            Vec::new(),
            Executors {
                claude: cpa_exec::claude::ClaudeExecutor::new("http://127.0.0.1:1").unwrap(),
                codex: Default::default(),
                devices: Default::default(),
                openai: Default::default(),
                google: Default::default(),
            },
        );
        let offsets: Vec<usize> = (0..4).map(|_| rt.next_pool_offset("a|openai|m", 3)).collect();
        assert_eq!(offsets, [0, 1, 2, 0]);
        assert_eq!(rt.next_pool_offset(" a|openai|m ", 3), 1, "keys are trimmed");
        assert_eq!(rt.next_pool_offset("b|openai|m", 3), 0, "keys rotate independently");
        assert_eq!(rt.next_pool_offset("c", 1), 0);
        assert_eq!(rt.next_pool_offset("c", 2), 0, "a single-entry pool does not advance");
        assert_eq!(rt.next_pool_offset("  ", 2), 0);
        rt.pool_offsets.lock().unwrap().insert("d".into(), 2_147_483_641);
        assert_eq!(rt.next_pool_offset("d", 3), 0, "the guard resets before use");
        assert_eq!(rt.next_pool_offset("d", 3), 1);
    }

    /// Go `Manager.persist` after `MarkResult`: the auth file gains `"disabled": false`
    /// in `json.Marshal` form (sorted keys, float64 numbers, HTML escaping, no newline)
    /// once, and is not rewritten while it already holds that JSON.
    #[test]
    fn first_use_persists_disabled_in_go_marshal_form() {
        let dir = std::env::temp_dir().join(format!(
            "persist-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("a.json");
        let original = "{\"type\":\"claude\",\"email\":\"a<b>@x.invalid\",\"n\":1.50,\"big\":1e3,\"nested\":{\"z\":1,\"a\":[true,null]}}\n";
        std::fs::write(&path, original).unwrap();
        let metadata: Map<String, Value> = serde_json::from_str(original).unwrap();
        let store = CredentialStore::new(vec![Credential::from_file(&dir, &path, metadata).unwrap()]);
        let revision = store.get("a.json").unwrap().revision;
        store
            .select(Selection::new("claude", "m"))
            .unwrap()
            .complete(Outcome::Success);
        let written = std::fs::read_to_string(&path).unwrap();
        assert_eq!(
            written,
            r#"{"big":1000,"disabled":false,"email":"a\u003cb\u003e@x.invalid","n":1.5,"nested":{"a":[true,null],"z":1},"type":"claude"}"#
        );
        let current = store.get("a.json").unwrap();
        assert_eq!(current.revision, revision, "a neutral write keeps the revision");
        assert_eq!(current.metadata.get("disabled"), Some(&Value::Bool(false)));
        assert_eq!(current.metadata.get("big"), Some(&Value::from(1000)));
        // The watcher's reload of the written file changes nothing.
        let reloaded: Map<String, Value> = serde_json::from_str(&written).unwrap();
        store.reconcile(vec![Credential::from_file(&dir, &path, reloaded).unwrap()]);
        assert_eq!(store.get("a.json").unwrap().revision, revision);
        // Same JSON in another layout is left alone.
        std::fs::write(&path, "{\"type\":\"claude\", \"disabled\":false}").unwrap();
        let mut metadata = Map::new();
        metadata.insert("type".into(), "claude".into());
        metadata.insert("disabled".into(), false.into());
        store.reconcile(vec![Credential::from_file(&dir, &path, metadata).unwrap()]);
        store
            .select(Selection::new("claude", "m"))
            .unwrap()
            .complete(Outcome::Success);
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "{\"type\":\"claude\", \"disabled\":false}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A result recorded after a refresh committed a newer revision must not write the
    /// older token back (Go persists under the manager lock with generation checks).
    #[test]
    fn stale_result_does_not_overwrite_a_refreshed_credential() {
        let dir = std::env::temp_dir().join(format!(
            "stale-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("a.json");
        std::fs::write(&path, r#"{"type":"claude","access_token":"old"}"#).unwrap();
        let metadata: Map<String, Value> = serde_json::from_str(r#"{"type":"claude","access_token":"old"}"#).unwrap();
        let store = CredentialStore::new(vec![Credential::from_file(&dir, &path, metadata).unwrap()]);
        let stale = store.get("a.json").unwrap();
        let mut patch = MetadataPatch::default();
        patch.set.insert("access_token".into(), "new".into());
        store.apply_patch("a.json", stale.revision, &patch).unwrap();
        let refreshed = std::fs::read_to_string(&path).unwrap();
        store.persist_credential(&stale);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), refreshed);
        assert!(refreshed.contains("\"new\"") && refreshed.contains("\"disabled\":false"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Go `save-cooldown-status`: a cooldown change writes `<auth-dir>/<file>.cds`, a new
    /// process restores it, reset clears it, and a file Go wrote restores too.
    #[test]
    fn cooldowns_persist_restore_and_reset_through_cds_files() {
        let dir = std::env::temp_dir().join(format!(
            "cds-rt-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let config = || {
            Config::parse(&format!(
                "auth-dir: {}\nrouting:\n  cooldown:\n    save-cooldown-status: true\n",
                dir.display()
            ))
            .unwrap()
        };
        let creds = || {
            let metadata = |p: &str| {
                let mut m = Map::new();
                m.insert("type".into(), p.into());
                m
            };
            vec![
                Credential::from_file(&dir, &dir.join("a.json"), metadata("claude")).unwrap(),
                Credential::from_file(&dir, &dir.join("b.json"), metadata("claude")).unwrap(),
            ]
        };
        let executors = || Executors {
            claude: cpa_exec::claude::ClaudeExecutor::new("http://127.0.0.1:1").unwrap(),
            codex: Default::default(),
            openai: Default::default(),
            google: Default::default(),
            devices: Default::default(),
        };
        let rt = crate::testing::runtime(config(), creds(), executors());
        let mut selection = Selection::new("claude", "m1");
        selection.exclude.push("b.json".into());
        let lease = rt.store.select(selection.clone()).unwrap();
        let mut quota = ExecError::local(429, FailureScope::Model, "rate limited");
        quota.retry_after = Some(Duration::from_secs(120));
        lease.complete(Outcome::Failure(quota));
        let file = dir.join("a.cds");
        let written = std::fs::read_to_string(&file).expect("cooldown written");
        #[cfg(unix)]
        assert_eq!(
            std::fs::metadata(&file).unwrap().permissions().mode() & 0o777,
            0o600,
            "Go os.CreateTemp mode"
        );
        assert!(
            written.contains("\"model\": \"m1\"") && written.contains("\"reason\": \"quota\""),
            "{written}"
        );
        assert!(!dir.join("b.cds").exists());
        // A successful attempt on another credential changes no cooldown: no rewrite.
        let modified = std::fs::metadata(&file).unwrap().modified().unwrap();
        let mut other = Selection::new("claude", "m1");
        other.exclude.push("a.json".into());
        rt.store.select(other).unwrap().complete(Outcome::Success);
        assert_eq!(std::fs::metadata(&file).unwrap().modified().unwrap(), modified);

        // A new process restores the cooldown.
        drop(rt);
        let rt = crate::testing::runtime(config(), creds(), executors());
        let a = rt.store.get("a.json").unwrap();
        let wait = rt
            .store
            .scheduler
            .lock()
            .unwrap()
            .wait(&a, "m1", Instant::now())
            .unwrap();
        assert!(
            wait > Duration::from_secs(100) && wait <= Duration::from_secs(120),
            "{wait:?}"
        );
        assert_eq!(rt.store.reset_cooldown("a.json"), ["m1"]);
        assert!(!file.exists(), "reset removes the stale file");

        // A file CLIProxyAPI wrote (local time with an offset) restores model and
        // credential-wide quota records; an expired record is dropped on rewrite.
        let at = |secs: i64| {
            (chrono::Utc::now() + chrono::Duration::seconds(secs))
                .with_timezone(&chrono::FixedOffset::east_opt(7200).unwrap())
                .to_rfc3339_opts(chrono::SecondsFormat::Nanos, false)
        };
        let go = format!(
            r#"{{"version":1,"auth_id":"b.json","provider":"claude","updated_at":"{now}","records":[
{{"provider":"claude","auth_id":"b.json","status":"cooling","next_retry_after":"{late}","reason":"credential_quota","quota":{{"exceeded":true,"reason":"credential_quota","next_recover_at":"{late}","observed_at":"0001-01-01T00:00:00Z"}},"last_error":{{"message":"credential quota","retryable":false,"http_status":429}},"updated_at":"{now}"}},
{{"provider":"claude","auth_id":"b.json","model":"m2","status":"cooling","next_retry_after":"{soon}","reason":"unauthorized","quota":{{"exceeded":false,"next_recover_at":"0001-01-01T00:00:00Z","observed_at":"0001-01-01T00:00:00Z"}},"last_error":{{"message":"unauthorized","retryable":false,"http_status":401}},"updated_at":"{now}"}},
{{"provider":"claude","auth_id":"b.json","model":"m4","status":"cooling","next_retry_after":"{soon}","reason":"quota","quota":{{"exceeded":true,"reason":"quota","next_recover_at":"{late}","observed_at":"0001-01-01T00:00:00Z"}},"updated_at":"{now}"}},
{{"provider":"claude","auth_id":"b.json","model":"m5","status":"cooling","next_retry_after":"{past}","reason":"quota","quota":{{"exceeded":true,"reason":"quota","next_recover_at":"{late}","observed_at":"0001-01-01T00:00:00Z"}},"updated_at":"{now}"}},
{{"provider":"claude","auth_id":"b.json","model":"m3","status":"cooling","next_retry_after":"{past}","reason":"boom","quota":{{"exceeded":false,"next_recover_at":"0001-01-01T00:00:00Z","observed_at":"0001-01-01T00:00:00Z"}},"updated_at":"{now}"}},
{{"provider":"claude","auth_id":"gone.json","model":"m1","status":"cooling","next_retry_after":"{late}","quota":{{"exceeded":false,"next_recover_at":"0001-01-01T00:00:00Z","observed_at":"0001-01-01T00:00:00Z"}},"updated_at":"{now}"}}]}}"#,
            now = at(0),
            late = at(600),
            soon = at(60),
            past = at(-5),
        );
        std::fs::write(dir.join("b.cds"), go).unwrap();
        drop(rt);
        let rt = crate::testing::runtime(config(), creds(), executors());
        let b = rt.store.get("b.json").unwrap();
        let now = Instant::now();
        let scheduler = rt.store.scheduler.lock().unwrap();
        let wait = |m: &str| scheduler.wait(&b, m, now).unwrap_or_default().as_secs();
        assert!((590..=600).contains(&wait("m9")), "credential quota blocks every model");
        assert!(scheduler.quota_cooling(&b, "m9", now));
        let records = scheduler.records(&b, now, std::time::SystemTime::now());
        drop(scheduler);
        let models: Vec<&str> = records.iter().map(|r| r.model.as_str()).collect();
        assert_eq!(
            models,
            ["", "m2", "m4"],
            "expired (by retry deadline) and unknown-credential records are dropped"
        );
        let m4 = records.iter().find(|r| r.model == "m4").unwrap();
        let left = m4
            .next_retry_after
            .unwrap()
            .duration_since(std::time::SystemTime::now())
            .unwrap();
        assert!(
            left > Duration::from_secs(590),
            "the later quota recovery wins: {left:?}"
        );
        let rewritten = std::fs::read_to_string(dir.join("b.cds")).unwrap();
        assert!(!rewritten.contains("gone.json") && !rewritten.contains("\"m3\""));

        // Turning the option off persists the final state and stops writing.
        rt.publish_config(Config::parse(&format!("auth-dir: {}\n", dir.display())).unwrap());
        rt.store.reset_cooldown("b.json");
        assert!(dir.join("b.cds").exists(), "no writes once disabled");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn preparation_waiter_observes_deletion_and_refresh_loop_is_replaceable() {
        let rt = Arc::new(crate::testing::runtime(
            Config::parse("").unwrap(),
            vec![cred("a.json", "claude", false)],
            Executors {
                claude: cpa_exec::claude::ClaudeExecutor::new("http://127.0.0.1:1").unwrap(),
                codex: Default::default(),
                devices: Default::default(),
                openai: Default::default(),
                google: Default::default(),
            },
        ));
        let lock = rt.store.prepare_lock("a.json");
        let guard = lock.lock().await;
        let worker = {
            let rt = rt.clone();
            tokio::spawn(async move { rt.prepare_credential("a.json", &rt.config(), None).await })
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

    /// The loop sets no timer while nothing can come due, and a credential added later
    /// is tried after the store change, not on a schedule.
    #[tokio::test]
    async fn refresh_loop_sleeps_until_the_store_changes() {
        let rt = Arc::new(crate::testing::runtime(
            Config::parse("").unwrap(),
            Vec::new(),
            Executors {
                claude: cpa_exec::claude::ClaudeExecutor::new("http://127.0.0.1:1").unwrap(),
                codex: Default::default(),
                devices: Default::default(),
                openai: Default::default(),
                google: Default::default(),
            },
        ));
        rt.start_auto_refresh();
        tokio::time::sleep(Duration::from_millis(50)).await;
        let mut due = cred("a.json", "claude", false);
        due.metadata.insert("refresh_token".into(), "fake-refresh".into());
        due.metadata.insert(
            "expired".into(),
            (chrono::Utc::now() - chrono::Duration::hours(1)).to_rfc3339().into(),
        );
        rt.store.reconcile(vec![due]);
        let added = rt.store.get("a.json").unwrap();
        // The refresh fails (no upstream; the OAuth client retries twice with 1 s and
        // 2 s pauses), which backs off for five minutes.
        tokio::time::timeout(Duration::from_secs(20), async {
            loop {
                let retry = rt.refresh_state.lock().unwrap().retry_at(&added);
                if retry.is_some_and(|at| at > Instant::now() + Duration::from_secs(200)) {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("the added credential was tried and backed off");
        rt.stop_auto_refresh();
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

    /// A remote lease ends once, never through the local scheduler, and only after the
    /// upstream body it was streaming has been dropped.
    #[tokio::test]
    async fn remote_leases_end_after_the_upstream_body_closes() {
        use std::sync::atomic::AtomicBool;
        struct Probe(Arc<AtomicBool>);
        impl Drop for Probe {
            fn drop(&mut self) {
                self.0.store(true, Ordering::SeqCst);
            }
        }
        let store = CredentialStore::new(vec![cred("a.json", "claude", false)]);
        let dropped = Arc::new(AtomicBool::new(false));
        let observed = Arc::new(Mutex::new(Vec::new()));
        let mut lease = store.select(sel("claude")).unwrap();
        let (seen, probe) = (observed.clone(), dropped.clone());
        lease.remote = Some(crate::remote::RemoteEnd {
            end: Some(Box::new(move || {
                seen.lock().unwrap().push(probe.load(Ordering::SeqCst));
                None
            })),
            releases: Default::default(),
            cancel: None,
        });
        let guard = Probe(dropped.clone());
        let err = ExecError::local(502, FailureScope::Transport, "boom");
        let inner = futures_util::stream::iter(vec![Ok(Bytes::from_static(b"a")), Err(err)])
            .chain(futures_util::stream::pending())
            .map(move |item| {
                let _ = &guard;
                item
            })
            .boxed();
        let mut stream = Completing::new(inner, lease);
        assert!(stream.next().await.unwrap().is_ok());
        assert!(observed.lock().unwrap().is_empty(), "still streaming");
        assert!(stream.next().await.unwrap().is_err());
        assert_eq!(
            *observed.lock().unwrap(),
            vec![true],
            "body dropped first, then one end"
        );
        drop(stream);
        assert_eq!(observed.lock().unwrap().len(), 1);
        assert_eq!(
            store.stats(),
            AttemptStats::default(),
            "the local scheduler saw nothing"
        );
    }
    /// Go binds the attempt's cancellation to the Home selection: a drain closes the
    /// upstream body and ends the lease while the client has stopped reading
    /// (backpressure), and the reader then sees the cancellation once.
    #[tokio::test]
    async fn a_drain_ends_a_remote_stream_the_client_stopped_reading() {
        use std::sync::atomic::AtomicBool;
        struct Probe(Arc<AtomicBool>);
        impl Drop for Probe {
            fn drop(&mut self) {
                self.0.store(true, Ordering::SeqCst);
            }
        }
        let store = CredentialStore::new(vec![cred("a.json", "claude", false)]);
        let dropped = Arc::new(AtomicBool::new(false));
        let observed = Arc::new(Mutex::new(Vec::new()));
        let mut lease = store.select(sel("claude")).unwrap();
        let (seen, probe) = (observed.clone(), dropped.clone());
        let (cancel_tx, cancel) = tokio::sync::watch::channel(false);
        lease.remote = Some(crate::remote::RemoteEnd {
            end: Some(Box::new(move || {
                seen.lock().unwrap().push(probe.load(Ordering::SeqCst));
                None
            })),
            releases: Default::default(),
            cancel: Some(cancel),
        });
        let guard = Probe(dropped.clone());
        let inner = futures_util::stream::iter(vec![Ok(Bytes::from_static(b"a"))])
            .chain(futures_util::stream::pending())
            .map(move |item| {
                let _ = &guard;
                item
            })
            .boxed();
        let mut stream = Completing::new(inner, lease);
        assert!(stream.next().await.unwrap().is_ok());
        // The client stops polling; the dispatcher drains.
        cancel_tx.send(true).unwrap();
        for _ in 0..100 {
            if !observed.lock().unwrap().is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(
            *observed.lock().unwrap(),
            vec![true],
            "body dropped first, then one end, without a poll"
        );
        let error = stream.next().await.unwrap().unwrap_err();
        assert_eq!(String::from_utf8_lossy(&error.body), "context canceled");
        assert!(stream.next().await.is_none());
        drop(stream);
        assert_eq!(observed.lock().unwrap().len(), 1);
    }
}
