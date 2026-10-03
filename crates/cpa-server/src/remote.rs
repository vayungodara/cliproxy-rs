//! Credentials dispatched by a control plane instead of the local scheduler (Go Home
//! mode: `pickHomeDispatchSelection` / `executeHomeOnce`). While a dispatcher is
//! installed every request routes to provider `home`, each pick asks the dispatcher for
//! one credential, and the credential's lease ends with a release the dispatcher
//! reports back. The dispatcher owns cooldowns and retry limits; nothing is recorded in
//! the local scheduler.

use std::sync::{Arc, Mutex, PoisonError};

use cpa_core::credential::Credential;
use cpa_core::exec::ExecError;
use axum::response::IntoResponse;
use futures_util::future::BoxFuture;

/// Attribute carrying the upstream model the dispatcher chose (Go
/// `homeUpstreamModelAttributeKey`).
pub const UPSTREAM_MODEL: &str = "home_upstream_model";
/// Set to `true` when responses report the route model instead (Go
/// `homeForceMappingAttributeKey`).
pub const FORCE_MAPPING: &str = "home_force_mapping";
/// The alias Home mapped from (Go `homeOriginalAliasAttributeKey`).
pub const ORIGINAL_ALIAS: &str = "home_original_alias";

/// One credential request (Go's RPOP request fields).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RemoteRequest {
    /// The route model as the client sent it (Home mode resolves nothing locally).
    pub model: String,
    pub session_id: String,
    pub parent_session_id: String,
    /// Downstream request headers; the dispatcher authenticates the client with them.
    pub headers: Vec<(String, String)>,
    /// 1 for the first pick of a retry round, then 2, 3, ... (Go `homeAuthCount`).
    pub count: i64,
    pub retry_round: i64,
    /// Credentials already tried this round.
    pub excluded: Vec<String>,
    pub pinned: String,
    pub request_id: String,
    /// `http`, `stream` or `websocket`.
    pub kind: &'static str,
}

/// Completes once the dispatcher acknowledged a release (bounded by the dispatcher).
pub type ReleaseWait = BoxFuture<'static, Result<(), String>>;

/// Ends a dispatched credential's lease exactly once; returns the acknowledgement to
/// wait for before the next pick of the same request, when there is one.
pub type EndLease = Box<dyn FnOnce() -> Option<ReleaseWait> + Send>;

/// A dispatched credential and the end of its lease.
pub struct RemoteGrant {
    pub credential: Credential,
    pub end: EndLease,
}

pub trait RemoteDispatch: Send + Sync + 'static {
    /// Go `HeartbeatOK`: requests are refused while the control plane is unreachable.
    fn available(&self) -> bool;
    /// One pick. Errors are the client-facing failure (status and Go's `code: message`).
    fn dispatch(&self, request: RemoteRequest) -> BoxFuture<'_, Result<RemoteGrant, ExecError>>;
}

/// Releases ended during one request, awaited before its next pick (Go
/// `endHomeSelectionBeforeRedispatch`).
#[derive(Clone, Default)]
pub struct PendingReleases(Arc<Mutex<Vec<ReleaseWait>>>);

impl PendingReleases {
    pub(crate) fn push(&self, wait: ReleaseWait) {
        self.0.lock().unwrap_or_else(PoisonError::into_inner).push(wait);
    }

    /// Waits for every release ended so far; the first failure is returned.
    pub(crate) async fn settle(&self) -> Result<(), String> {
        let waits = std::mem::take(&mut *self.0.lock().unwrap_or_else(PoisonError::into_inner));
        for wait in waits {
            wait.await?;
        }
        Ok(())
    }
}

/// The lease side of a dispatched credential: ends it on the first report.
pub(crate) struct RemoteEnd {
    pub end: Option<EndLease>,
    pub releases: PendingReleases,
}

impl RemoteEnd {
    pub(crate) fn finish(&mut self) {
        if let Some(end) = self.end.take()
            && let Some(wait) = end()
        {
            self.releases.push(wait);
        }
    }
}

/// Go `homeHeartbeatMiddleware`: while a dispatcher is installed and its control plane
/// is unreachable, every route but management answers 503 with an empty body.
pub async fn gate(
    axum::extract::State(rt): axum::extract::State<Arc<crate::Runtime>>,
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    let path = request.uri().path();
    let exempt = ["/v0/management", "/v8/management"]
        .iter()
        .any(|base| path == *base || path.strip_prefix(base).is_some_and(|rest| rest.starts_with('/')))
        || path.starts_with("/v0/resource/plugins/")
        || path == "/management.html";
    if !exempt
        && let Some(remote) = rt.remote_dispatch()
        && !remote.available()
    {
        return axum::http::StatusCode::SERVICE_UNAVAILABLE.into_response();
    }
    next.run(request).await
}
