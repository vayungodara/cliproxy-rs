//! Remembered calls (sideband.go `sessionStore`): what the sideband and hangup need
//! after `POST /v1/live` returned, keyed by call ID.
//!
//! A call expires an hour after it was stored or last released. One sideband may claim
//! it at a time; a claimed call does not expire. Completing a call (hangup, sideband end,
//! replacement, shutdown) closes everything registered on it.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, PoisonError, Weak};
use std::time::Duration;

use tokio::task::AbortHandle;

/// `sessionLifetime`.
const LIFETIME: Duration = Duration::from_secs(3600);

/// One remembered call (`liveSession`).
#[derive(Default)]
pub(super) struct Call {
    pub call_id: String,
    /// The credential that created it; the sideband and hangup are pinned to it.
    pub auth_id: String,
    /// The request's session (`$CPA-SESSION-ID` for the sideband and hangup).
    pub session_id: String,
    /// The upstream model the call was selected for (Home picks and results).
    pub model: String,
    /// The Home pick the call keeps (Go `liveSession.homeSelection`), in Home mode.
    pub home: Option<Arc<HomeHold>>,
    /// Who created it: only the same client key may join or hang up.
    pub owner_key: String,
    pub owner_provider: String,
    /// The ephemeral key's `sess_` identity when one created it.
    pub secret_principal: String,
    pub resources: Resources,
    /// The relayed media session, when the media relay handled the call.
    pub media: Option<Arc<dyn super::relay::MediaSession>>,
    /// Assigned by [`Calls::put`]; tells a stale handle from the current call.
    pub token: u64,
}

/// Tasks tied to a call's lifetime (the sideband relay); aborted when the call ends.
#[derive(Debug, Default)]
pub(super) struct Resources(Mutex<(bool, Vec<AbortHandle>)>);

impl Resources {
    pub fn add(&self, handle: AbortHandle) {
        let mut state = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        if state.0 {
            handle.abort();
        } else {
            state.1.push(handle);
        }
    }

    fn close(&self) {
        let handles = {
            let mut state = self.0.lock().unwrap_or_else(PoisonError::into_inner);
            state.0 = true;
            std::mem::take(&mut state.1)
        };
        for handle in handles {
            handle.abort();
        }
    }
}

/// A Home pick a call keeps for its lifetime (Go `liveSession.homeSelection`, retained):
/// the sideband and hangup run on its credential. Ending the call ends it; Home
/// draining it closes what was bound to it.
pub(super) struct HomeHold {
    lease: Mutex<Option<crate::runtime::Lease>>,
    /// Relays bound to the selection (Go `selection.Bind`), closed when it ends.
    bound: Resources,
}

impl HomeHold {
    pub fn new(lease: crate::runtime::Lease) -> Arc<Self> {
        Arc::new(Self {
            lease: Mutex::new(Some(lease)),
            bound: Resources::default(),
        })
    }

    /// Go `selection.Active()`: the credential while the pick has neither ended nor
    /// been drained.
    pub fn active(&self) -> Option<Arc<cpa_core::credential::Credential>> {
        let lease = self.lease.lock().unwrap_or_else(PoisonError::into_inner);
        lease
            .as_ref()
            .filter(|lease| !lease.remote_cancel_requested())
            .map(|lease| lease.credential.clone())
    }

    /// Resolves when Home drains the pick; `None` once it ended.
    pub fn drained(&self) -> Option<futures_util::future::BoxFuture<'static, ()>> {
        let lease = self.lease.lock().unwrap_or_else(PoisonError::into_inner);
        lease.as_ref().and_then(crate::runtime::Lease::remote_cancelled)
    }

    /// Go `selection.Bind`: `handle` is aborted when the pick ends.
    pub fn bind(&self, handle: AbortHandle) {
        self.bound.add(handle);
    }

    /// Go `selection.End`: closes what was bound to the pick and hands back its lease,
    /// which ends when dropped (the caller drops it once the call's media closed).
    pub fn end(&self) -> Option<crate::runtime::Lease> {
        let lease = self.lease.lock().unwrap_or_else(PoisonError::into_inner).take();
        self.bound.close();
        lease
    }
}

struct Entry {
    call: Arc<Call>,
    claimed: bool,
    timer: Option<AbortHandle>,
}

pub(super) enum Claim {
    Missing,
    Busy,
    Acquired(Arc<Call>),
}

pub(super) struct Calls {
    entries: Mutex<(u64, HashMap<String, Entry>)>,
    lifetime: Duration,
    me: Weak<Calls>,
}

impl Calls {
    /// Calls live an hour unless claimed (`sessionLifetime`).
    pub fn new() -> Arc<Self> {
        Self::new_shared(LIFETIME)
    }

    pub fn new_shared(lifetime: Duration) -> Arc<Self> {
        Arc::new_cyclic(|me| Self {
            entries: Mutex::default(),
            lifetime,
            me: me.clone(),
        })
    }

    fn timer(&self, call_id: &str, token: u64) -> Option<AbortHandle> {
        let me = self.me.clone();
        let lifetime = self.lifetime;
        let call_id = call_id.to_owned();
        let task = tokio::runtime::Handle::try_current().ok()?.spawn(async move {
            tokio::time::sleep(lifetime).await;
            if let Some(calls) = me.upgrade() {
                calls.expire(&call_id, token);
            }
        });
        Some(task.abort_handle())
    }

    /// Stores `call` under `call_id`, replacing (and ending) any previous call there.
    /// `None` for an invalid ID.
    pub fn put(&self, call_id: &str, mut call: Call) -> Option<Arc<Call>> {
        if !cpa_exec::codex_live::valid_call_id(call_id) {
            return None;
        }
        let mut entries = self.entries.lock().unwrap_or_else(PoisonError::into_inner);
        entries.0 += 1;
        call.call_id = call_id.to_owned();
        call.token = entries.0;
        let call = Arc::new(call);
        let timer = self.timer(call_id, call.token);
        let previous = entries.1.insert(
            call_id.to_owned(),
            Entry {
                call: call.clone(),
                claimed: false,
                timer,
            },
        );
        drop(entries);
        if let Some(previous) = previous {
            end(previous, "session_replaced");
        }
        Some(call)
    }

    pub fn claim(&self, call_id: &str) -> Claim {
        if !cpa_exec::codex_live::valid_call_id(call_id) {
            return Claim::Missing;
        }
        let mut entries = self.entries.lock().unwrap_or_else(PoisonError::into_inner);
        let Some(entry) = entries.1.get_mut(call_id) else {
            return Claim::Missing;
        };
        if entry.claimed {
            return Claim::Busy;
        }
        entry.claimed = true;
        if let Some(timer) = entry.timer.take() {
            timer.abort();
        }
        Claim::Acquired(entry.call.clone())
    }

    /// Gives a claimed call back; its hour starts again.
    pub fn release(&self, call: &Call) {
        let mut entries = self.entries.lock().unwrap_or_else(PoisonError::into_inner);
        let Some(entry) = entries.1.get_mut(&call.call_id) else {
            return;
        };
        if entry.call.token != call.token || !entry.claimed {
            return;
        }
        entry.claimed = false;
        entry.timer = self.timer(&call.call_id, call.token);
    }

    /// Forgets the call (if it is still this one) and ends it.
    pub fn complete(&self, call: &Call, reason: &str) {
        self.complete_token(&call.call_id, call.token, reason);
    }

    /// [`Calls::complete`] by identity, for callbacks that must not keep the call alive.
    pub fn complete_token(&self, call_id: &str, token: u64, reason: &str) {
        let removed = {
            let mut entries = self.entries.lock().unwrap_or_else(PoisonError::into_inner);
            match entries.1.get(call_id) {
                Some(entry) if entry.call.token == token => entries.1.remove(call_id),
                _ => None,
            }
        };
        if let Some(entry) = removed {
            end(entry, reason);
        }
    }

    pub fn peek(&self, call_id: &str) -> Option<Arc<Call>> {
        let entries = self.entries.lock().unwrap_or_else(PoisonError::into_inner);
        entries.1.get(call_id).map(|e| e.call.clone())
    }

    pub fn close_all(&self, reason: &str) {
        let drained: Vec<Entry> = {
            let mut entries = self.entries.lock().unwrap_or_else(PoisonError::into_inner);
            entries.1.drain().map(|(_, e)| e).collect()
        };
        for entry in drained {
            end(entry, reason);
        }
    }

    fn expire(&self, call_id: &str, token: u64) {
        let removed = {
            let mut entries = self.entries.lock().unwrap_or_else(PoisonError::into_inner);
            match entries.1.get(call_id) {
                Some(entry) if entry.call.token == token && !entry.claimed => entries.1.remove(call_id),
                _ => None,
            }
        };
        if let Some(entry) = removed {
            end(entry, "session_expired");
        }
    }
}

/// `endLiveSession`.
fn end(entry: Entry, reason: &str) {
    if let Some(timer) = entry.timer {
        timer.abort();
    }
    tracing::debug!(call_id = %entry.call.call_id, reason, "codex live call ended");
    // Go `endLiveSession`: resources, then media, then the Home selection, which is
    // released only once the media's peers closed.
    // ponytail: aborted sideband tasks drop their sockets when next polled, right after.
    entry.call.resources.close();
    let lease = entry.call.home.as_ref().and_then(|home| home.end());
    match &entry.call.media {
        Some(media) => {
            media.close(reason);
            media.after_close(Box::new(move || drop(lease)));
        }
        None => drop(lease),
    }
}

/// A sideband's claim on a call: dropped unconsumed it releases the call, consumed it
/// completes it (Go's deferred `consumeSession` branch).
pub(super) struct ClaimGuard {
    calls: Arc<Calls>,
    pub call: Arc<Call>,
    pub consume: bool,
}

impl ClaimGuard {
    pub fn new(calls: Arc<Calls>, call: Arc<Call>) -> Self {
        Self {
            calls,
            call,
            consume: false,
        }
    }
}

impl Drop for ClaimGuard {
    fn drop(&mut self) {
        if self.consume {
            self.calls.complete(&self.call, "session_closed");
        } else {
            self.calls.release(&self.call);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn call(auth: &str) -> Call {
        Call {
            auth_id: auth.into(),
            ..Call::default()
        }
    }

    /// Go `TestSessionStoreClaimsAndExpiresSessions`.
    #[tokio::test]
    async fn claims_release_and_expire() {
        let calls = Calls::new_shared(Duration::from_millis(40));
        calls.put("call-claim", call("auth-1")).unwrap();
        let Claim::Acquired(claimed) = calls.claim("call-claim") else {
            panic!("first claim")
        };
        assert!(matches!(calls.claim("call-claim"), Claim::Busy));
        tokio::time::sleep(Duration::from_millis(80)).await;
        assert!(calls.peek("call-claim").is_some(), "a claimed call never expires");
        calls.release(&claimed);
        assert!(matches!(calls.claim("call-claim"), Claim::Acquired(_)));
        calls.release(&claimed);
        tokio::time::sleep(Duration::from_millis(80)).await;
        assert!(calls.peek("call-claim").is_none(), "released call expired");
        assert!(matches!(calls.claim("call-claim"), Claim::Missing));
    }

    #[tokio::test]
    async fn replacement_and_completion_end_resources() {
        let calls = Calls::new_shared(Duration::from_secs(60));
        let first = calls.put("call-1", call("a")).unwrap();
        let task = tokio::spawn(std::future::pending::<()>());
        first.resources.add(task.abort_handle());
        let second = calls.put("call-1", call("b")).unwrap();
        assert!(task.await.unwrap_err().is_cancelled(), "replaced call's relay aborted");
        // A stale completion (old token) must not remove the replacement.
        calls.complete(&first, "late");
        assert_eq!(calls.peek("call-1").unwrap().auth_id, "b");
        calls.complete(&second, "client_hangup");
        assert!(calls.peek("call-1").is_none());
        // Resources added after the end are aborted at once.
        let late = tokio::spawn(std::future::pending::<()>());
        second.resources.add(late.abort_handle());
        assert!(late.await.unwrap_err().is_cancelled());
        assert!(calls.put("bad id", call("c")).is_none());
        assert!(calls.put(&"a".repeat(129), call("c")).is_none());
    }

    #[tokio::test]
    async fn guard_releases_unless_consumed() {
        let calls = Calls::new_shared(Duration::from_secs(60));
        calls.put("c", call("a")).unwrap();
        let Claim::Acquired(claimed) = calls.claim("c") else {
            panic!()
        };
        drop(ClaimGuard::new(calls.clone(), claimed));
        let Claim::Acquired(claimed) = calls.claim("c") else {
            panic!("released by the guard")
        };
        let mut guard = ClaimGuard::new(calls.clone(), claimed);
        guard.consume = true;
        drop(guard);
        assert!(calls.peek("c").is_none(), "consumed claim completes the call");
        calls.put("d", call("a")).unwrap();
        calls.close_all("server_stopped");
        assert!(calls.peek("d").is_none());
    }
}
