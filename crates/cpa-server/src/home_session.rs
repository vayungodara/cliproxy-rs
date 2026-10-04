//! Home picks kept by pooled upstream sockets (Go `HomeDispatchSelection` as an
//! `ExecutionLifecycle`, `retainHomeWebsocketSelection` and
//! `retainedHomeSessionSelection`).
//!
//! A downstream WebSocket turn on a Home credential hands its pick to the executor as
//! a [`Pick`]. When the executor keeps the upstream socket for later turns it retains
//! the pick, and after the turn succeeds the pick takes the lease over from the
//! attempt: the credential stays leased while the socket is open. The pick ends, and
//! the lease is released, when the socket is invalidated, replaced or closed, when
//! Home drains it (the socket closes first), or when the downstream connection ends.
//! The next turn of the same session runs on a still-active pick instead of asking
//! Home again.

use std::sync::{Arc, Mutex, PoisonError, Weak};

use cpa_core::exec::{Retainable, SessionLease};

use crate::runtime::Lease;

/// One attempt's Home pick as the executor sees it.
#[derive(Default)]
pub(crate) struct Pick(Mutex<State>);

#[derive(Default)]
struct State {
    retained: bool,
    ended: bool,
    /// Closes the socket that retained the pick.
    close: Option<Box<dyn FnOnce() + Send>>,
    /// Held once the turn succeeded on a retaining socket.
    lease: Option<Lease>,
    /// Closes the pick when Home drains its lease.
    watcher: Option<tokio::task::AbortHandle>,
}

impl Retainable for Pick {
    fn retain(&self, close: Box<dyn FnOnce() + Send>) -> bool {
        let mut state = self.state();
        if state.ended {
            return false;
        }
        state.retained = true;
        state.close = Some(close);
        true
    }

    fn end(&self) {
        let (lease, watcher) = {
            let mut state = self.state();
            state.ended = true;
            state.close = None;
            (state.lease.take(), state.watcher.take())
        };
        if let Some(watcher) = watcher {
            watcher.abort();
        }
        drop(lease);
    }
}

impl Pick {
    fn state(&self) -> std::sync::MutexGuard<'_, State> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }

    pub(crate) fn lease(self: &Arc<Self>) -> SessionLease {
        SessionLease(self.clone())
    }

    /// After a successful turn: the pick keeps `lease` when a socket retained it and
    /// still holds it. Otherwise the lease comes back to the attempt.
    pub(crate) fn keep(self: &Arc<Self>, lease: Lease) -> Option<Lease> {
        let drained = lease.remote_cancelled();
        let mut state = self.state();
        if !state.retained || state.ended {
            return Some(lease);
        }
        state.lease = Some(lease);
        if let Some(drained) = drained {
            let pick: Weak<Self> = Arc::downgrade(self);
            let watcher = tokio::spawn(async move {
                drained.await;
                if let Some(pick) = pick.upgrade() {
                    pick.close();
                }
            });
            state.watcher = Some(watcher.abort_handle());
        }
        None
    }

    /// Go `End` from the request side: closes the retaining socket, then releases the
    /// lease. Idempotent.
    pub(crate) fn close(&self) {
        let (close, lease, watcher) = {
            let mut state = self.state();
            state.ended = true;
            (state.close.take(), state.lease.take(), state.watcher.take())
        };
        if let Some(close) = close {
            close();
        }
        drop(lease);
        if let Some(watcher) = watcher {
            watcher.abort();
        }
    }

    fn release_into(&self, releases: &crate::remote::PendingReleases) {
        if let Some(lease) = self.state().lease.as_mut() {
            lease.release_into(releases);
        }
    }

    /// Lends the kept lease to the session's next turn, which runs on this same pick:
    /// the socket stays bound to it.
    fn lend(&self) -> Option<Lease> {
        let (lease, watcher) = {
            let mut state = self.state();
            if state.ended
                || !state
                    .lease
                    .as_ref()
                    .is_some_and(|lease| !lease.remote_cancel_requested())
            {
                return None;
            }
            (state.lease.take(), state.watcher.take())
        };
        if let Some(watcher) = watcher {
            watcher.abort();
        }
        lease
    }
}

/// Closes a pick unless the turn handed its lease to it; dropped before the attempt's
/// lease, so a socket closes before its credential is released.
pub(crate) struct PickGuard(Option<Arc<Pick>>);

impl PickGuard {
    pub(crate) fn new(pick: Arc<Pick>) -> Self {
        Self(Some(pick))
    }

    /// The pick holds the lease now; it no longer ends with the attempt.
    pub(crate) fn defuse(&mut self) -> Option<Arc<Pick>> {
        self.0.take()
    }
}

impl Drop for PickGuard {
    fn drop(&mut self) {
        if let Some(pick) = self.0.take() {
            pick.close();
        }
    }
}

struct Kept {
    pick: Arc<Pick>,
    credential: String,
    /// Canonical route model.
    route: String,
    /// The client key Home authenticated when it granted the pick.
    user_key: String,
    /// Home's request-retry limit with the pick (Go `selection.requestRetry`).
    request_retry: Option<i64>,
    /// The lease is out with the next turn's attempt.
    lent: bool,
}

/// The pick a downstream WebSocket connection keeps between turns (Go
/// `homeSessionSelections` for one session).
#[derive(Default)]
pub struct SessionHome {
    kept: Mutex<Option<Kept>>,
    /// Home's request-retry limit with the session's latest fresh pick.
    granted_retry: Mutex<Option<i64>>,
}

impl SessionHome {
    fn slot(&self) -> std::sync::MutexGuard<'_, Option<Kept>> {
        self.kept.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Records the request-retry limit Home sent with a fresh pick, which a kept pick
    /// carries to later turns.
    pub(crate) fn granted(&self, request_retry: Option<i64>) {
        *self.granted_retry.lock().unwrap_or_else(PoisonError::into_inner) = request_retry;
    }

    /// Go `retainHomeWebsocketSelection`: a pick kept for a previous turn ends
    /// (`target_replaced`).
    pub(crate) fn keep(&self, pick: Arc<Pick>, credential: &str, route: &str, user_key: String) {
        let request_retry = *self.granted_retry.lock().unwrap_or_else(PoisonError::into_inner);
        let previous = self.slot().replace(Kept {
            pick,
            credential: credential.to_owned(),
            route: crate::scheduler::canonical_model(route).to_owned(),
            user_key,
            request_retry,
            lent: false,
        });
        if let Some(previous) = previous {
            previous.pick.close();
        }
    }

    /// Go `retainedHomeSessionSelection`: the kept lease, its client key and Home's
    /// request-retry limit when this
    /// pick may run on it (a first pick of the first round, same route, a credential
    /// neither excluded nor other than the pinned one, not drained). Any other kept
    /// pick ends (`target_changed`); its release joins `releases`.
    pub(crate) fn reuse(
        &self,
        first_pick: bool,
        route: &str,
        excluded: &[String],
        pinned: Option<&str>,
        releases: &crate::remote::PendingReleases,
    ) -> Option<(Lease, String, Option<i64>)> {
        let mut slot = self.slot();
        let kept = slot.take()?;
        kept.pick.release_into(releases);
        let route = crate::scheduler::canonical_model(route);
        let matches = first_pick
            && !kept.lent
            && !route.is_empty()
            && kept.route == route
            && !excluded.contains(&kept.credential)
            && pinned.is_none_or(|id| id.is_empty() || id == kept.credential);
        if matches && let Some(lease) = kept.pick.lend() {
            let (user_key, request_retry) = (kept.user_key.clone(), kept.request_retry);
            *slot = Some(Kept { lent: true, ..kept });
            return Some((lease, user_key, request_retry));
        }
        drop(slot);
        kept.pick.close();
        None
    }

    /// The pick for an attempt on `credential`: the lent one when its lease came back
    /// for this attempt, else a new one. A lent pick the attempt does not take ends.
    pub(crate) fn pick(&self, credential: &str) -> Arc<Pick> {
        let lent = {
            let mut slot = self.slot();
            match slot.take() {
                Some(kept) if kept.lent => Some(kept),
                other => {
                    *slot = other;
                    None
                }
            }
        };
        match lent {
            Some(kept) if kept.credential == credential => kept.pick,
            Some(kept) => {
                kept.pick.close();
                Arc::default()
            }
            None => Arc::default(),
        }
    }

    /// The downstream connection ended (Go `CloseExecutionSession`).
    pub(crate) fn close(&self) {
        if let Some(kept) = self.slot().take() {
            kept.pick.close();
        }
    }
}
