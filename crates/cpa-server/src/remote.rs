//! Credentials dispatched by a control plane instead of the local scheduler (Go Home
//! mode: `pickHomeDispatchSelection` / `executeHomeOnce`). While a dispatcher is
//! installed every request routes to provider `home`, each pick asks the dispatcher for
//! one credential, and the credential's lease ends with a release the dispatcher
//! reports back. The dispatcher owns cooldowns and retry limits; nothing is recorded in
//! the local scheduler.

use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use axum::response::IntoResponse;
use cpa_core::credential::Credential;
use cpa_core::exec::ExecError;
use futures_util::future::BoxFuture;

/// Attribute carrying the upstream model the dispatcher chose (Go
/// `homeUpstreamModelAttributeKey`).
pub const UPSTREAM_MODEL: &str = "home_upstream_model";
/// Set to `true` when responses report the route model instead (Go
/// `homeForceMappingAttributeKey`).
pub const FORCE_MAPPING: &str = "home_force_mapping";
/// The alias Home mapped from (Go `homeOriginalAliasAttributeKey`).
pub const ORIGINAL_ALIAS: &str = "home_original_alias";
/// The dispatched auth's own `provider` field (Go `Auth.Provider`; the selection
/// provider is its lower-case form). The credential's provider is the executor key.
pub const PROVIDER: &str = cpa_core::config::credentials::HOME_PROVIDER;
/// Home's definition of the dispatched model, as sent (Go `homeDispatchModelInfo`):
/// [`crate::capabilities::bind_home`] makes it the attempt's resolved model.
pub const MODEL_INFO: &str = "home_model_info";
/// The session the dispatcher sent for this pick (Go
/// `HomeDispatchSelection.CanonicalSessionID`); usage records report it.
pub const SESSION: &str = "home_session_id";
/// Its parent (Go `HomeDispatchSelection.ParentSessionID`).
pub const PARENT_SESSION: &str = "home_parent_session_id";

/// Go `HomeDispatchSelection.Provider`: the dispatched auth's provider, lower-cased.
pub fn selection_provider(credential: &Credential) -> String {
    credential
        .attributes
        .get(PROVIDER)
        .map_or(credential.provider.as_str(), String::as_str)
        .trim()
        .to_lowercase()
}

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
    /// Go `credentialPolicyFromContext`: narrows Home's choice (`codex_alpha_search_v1`),
    /// empty for inference.
    pub credential_policy: String,
}

/// Completes once the dispatcher acknowledged a release (bounded by the dispatcher).
pub type ReleaseWait = BoxFuture<'static, Result<(), String>>;

/// Ends a dispatched credential's lease exactly once; returns the acknowledgement to
/// wait for before the next pick of the same request, when there is one.
pub type EndLease = Box<dyn FnOnce() -> Option<ReleaseWait> + Send>;

/// Set to `true` when the dispatcher cancels the execution (Go: a draining registry
/// closes the selection's bound resources, cancelling its attempt contexts).
pub type CancelSignal = tokio::sync::watch::Receiver<bool>;

/// Resolves once `signal` is set; never when its sender is gone without setting it.
pub(crate) fn cancelled(mut signal: CancelSignal) -> BoxFuture<'static, ()> {
    Box::pin(async move {
        if signal.wait_for(|cancelled| *cancelled).await.is_err() {
            std::future::pending::<()>().await;
        }
    })
}

/// The error an execution cancelled by the dispatcher ends with (Go `context.Canceled`).
pub(crate) fn cancelled_error() -> ExecError {
    ExecError::local(502, cpa_core::exec::FailureScope::Transport, "context canceled")
}

/// Go `clienterror.StatusClientClosedRequest`: what `HTTPStatusFromErrorOr` makes of a
/// cancelled context, so of a request Home's drain stopped.
pub(crate) const CLIENT_CLOSED: u16 = 499;

/// The error Go's `http.Client` returns for a request its cancelled context stopped
/// (`*url.Error`): `Post "<url>": context canceled`.
pub(crate) fn cancelled_request(method: &str, url: &str) -> String {
    let mut op = method.to_ascii_lowercase();
    if let Some(first) = op.get_mut(..1) {
        first.make_ascii_uppercase();
    }
    format!("{op} {}: context canceled", cpa_common::gostr::quote(url))
}

/// A dispatched credential and the end of its lease.
pub struct RemoteGrant {
    pub credential: Credential,
    pub end: EndLease,
    /// Cancels the execution running on this credential.
    pub cancel: Option<CancelSignal>,
    /// The request-retry limit Home set for this request (Go `selection.requestRetry`).
    pub request_retry: Option<i64>,
    /// The client key Home authenticated (Go `dispatch.UserAPIKey`), trimmed; empty
    /// when Home sent none. Private request context, like `Caller::principal`.
    pub user_api_key: String,
}

/// How a failed pick takes part in retries (Go's typed Home errors).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RemoteErrorKind {
    Plain,
    /// Go `homeDispatchRetryAfterError` (`model_cooldown`).
    Cooldown {
        retry_after: Option<Duration>,
        request_retry: Option<i64>,
    },
    /// Go `HomeConcurrencyBusyError`: never retried; `header` is its safe `Retry-After`.
    Busy {
        header: Option<u64>,
    },
}

/// A failed pick: the client-facing error (status and Go's `code: message`), its code
/// and its retry behaviour.
#[derive(Debug, Clone)]
pub struct RemoteError {
    pub error: ExecError,
    pub code: String,
    pub kind: RemoteErrorKind,
}

impl RemoteError {
    pub fn plain(error: ExecError, code: &str) -> Self {
        Self {
            error,
            code: code.to_owned(),
            kind: RemoteErrorKind::Plain,
        }
    }
}

/// Why the control plane's model catalog is not available.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModelsError {
    /// No live control-plane client (Go `home.Current() == nil`).
    Unavailable,
    /// The request failed; the text is Go's error.
    Failed(String),
}

pub trait RemoteDispatch: Send + Sync + 'static {
    /// Go `HeartbeatOK`: requests are refused while the control plane is unreachable.
    fn available(&self) -> bool;
    /// One pick.
    fn dispatch(&self, request: RemoteRequest) -> BoxFuture<'_, Result<RemoteGrant, RemoteError>>;
    /// Go `GetModels`: the catalog for the client these request headers and query
    /// parameters authenticate (raw Home payload).
    fn models(
        &self,
        headers: Vec<(String, String)>,
        query: Vec<(String, String)>,
    ) -> BoxFuture<'_, Result<Vec<u8>, ModelsError>>;

    /// Go RPushRequestLog. Unavailable Home lifetimes discard the log, never
    /// fall back to writing it locally. Test/internal dispatchers may ignore it.
    fn request_log(&self, payload: Vec<u8>) -> BoxFuture<'_, Result<(), String>> {
        let _ = payload;
        Box::pin(async { Ok(()) })
    }
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

/// A Home selection a handler could not make, as Go's handlers write it:
/// `HTTPStatusFromErrorOr(err, 503)`, `err.Error()` and the safe `Retry-After` seconds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Refusal {
    pub status: u16,
    pub text: String,
    pub retry_after: Option<u64>,
}

impl Refusal {
    fn not_found(message: &str) -> Self {
        Self {
            status: 503,
            text: format!("auth_not_found: {message}"),
            retry_after: None,
        }
    }
}

/// Go `SelectHomeAuthByKind` and `SelectHomeAuthWithCredentialPolicy`: one Home pick for
/// a handler outside the dispatch loop. A pick `accept` refuses is released, and the
/// release acknowledged, before the next pick, which excludes it; refusing the same
/// credential twice ends the selection.
pub(crate) async fn select(
    rt: &crate::Runtime,
    remote: &dyn RemoteDispatch,
    selection: crate::runtime::Selection,
    request: RemoteRequest,
    accept: impl Fn(&Credential) -> bool,
) -> Result<crate::runtime::Lease, Refusal> {
    let releases = PendingReleases::default();
    let mut tried: Vec<String> = Vec::new();
    let mut count = request.count.max(1);
    loop {
        let pick = RemoteRequest {
            count,
            excluded: tried.clone(),
            ..request.clone()
        };
        let (lease, _, _) = rt
            .acquire_remote(remote, selection.clone(), pick, &releases)
            .await
            .map_err(|failed| Refusal {
                status: failed.error.status,
                text: String::from_utf8_lossy(&failed.error.body).into_owned(),
                retry_after: match failed.kind {
                    RemoteErrorKind::Cooldown { retry_after, .. } => {
                        retry_after.map(|d| d.as_secs() + u64::from(d.subsec_nanos() > 0))
                    }
                    RemoteErrorKind::Busy { header } => header,
                    RemoteErrorKind::Plain => None,
                },
            })?;
        let credential = lease.credential.clone();
        if accept(&credential) {
            return Ok(lease);
        }
        // Go `endHomeSelectionBeforeRedispatch`.
        drop(lease);
        if let Err(message) = releases.settle().await {
            return Err(Refusal {
                status: 503,
                text: format!("home_unavailable: Home did not acknowledge credential release: {message}"),
                retry_after: None,
            });
        }
        let id = credential.id.trim().to_owned();
        if id.is_empty() {
            return Err(Refusal::not_found("selected auth has no ID"));
        }
        if tried.contains(&id) {
            return Err(Refusal::not_found("selector repeatedly returned an ineligible auth"));
        }
        tried.push(id);
        count += 1;
    }
}

/// The lease side of a dispatched credential: ends it on the first report.
pub(crate) struct RemoteEnd {
    pub end: Option<EndLease>,
    pub releases: PendingReleases,
    pub cancel: Option<CancelSignal>,
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

tokio::task_local! {
    /// The `?key=` (else `?auth_token=`) value of the request being served (Go
    /// `homeQueryCredentialFromContext` reads gin's query).
    static QUERY_CREDENTIAL: String;
}

/// `fut` with this request's query credential, for dispatch work that outlives the
/// gate (a keep-alive moves the unfinished call into the response body).
pub(crate) fn keep_query_credential<F: std::future::Future>(fut: F) -> impl std::future::Future<Output = F::Output> {
    let credential = QUERY_CREDENTIAL.try_with(Clone::clone).unwrap_or_default();
    QUERY_CREDENTIAL.scope(credential, fut)
}

/// Go `homeDispatchHeaders`: the request headers Home authenticates the client with;
/// a query credential is added as X-Goog-Api-Key when no credential header was sent:
/// the request's `?key=`/`?auth_token=`, else the query key `caller` authenticated with
/// (Go's `accessMetadata` branch; it also serves WebSocket sessions, which run after
/// their upgrade request).
pub(crate) fn home_headers(
    headers: &axum::http::HeaderMap,
    caller: Option<&cpa_core::exec::Caller>,
) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = headers
        .iter()
        .map(|(name, value)| {
            (
                name.as_str().to_owned(),
                String::from_utf8_lossy(value.as_bytes()).into_owned(),
            )
        })
        .collect();
    let mut credential = QUERY_CREDENTIAL.try_with(Clone::clone).unwrap_or_default();
    if credential.is_empty()
        && let Some(caller) = caller.filter(|c| matches!(c.source, "query-key" | "query-auth-token"))
    {
        credential = caller.principal.trim().to_owned();
    }
    let has_header = ["authorization", "x-goog-api-key", "x-api-key"]
        .iter()
        .any(|name| headers.get(*name).is_some_and(|v| !v.is_empty()));
    if !credential.is_empty() && !has_header {
        out.push(("x-goog-api-key".to_owned(), credential));
    }
    out
}

/// The query credential Go reads for Home: `key`, else `auth_token`, trimmed.
fn query_credential(query: &str) -> String {
    ["key", "auth_token"]
        .iter()
        .map(|name| {
            crate::access::query_get(query, name)
                .map(|v| String::from_utf8_lossy(&v).trim().to_owned())
                .unwrap_or_default()
        })
        .find(|v| !v.is_empty())
        .unwrap_or_default()
}

/// Go `homeHeartbeatMiddleware`: while a dispatcher is installed and its control plane
/// is unreachable, every route but management answers 503 with an empty body.
/// Inside, the request's query credential is available to `home_headers`.
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
    let credential = query_credential(request.uri().query().unwrap_or_default());
    QUERY_CREDENTIAL.scope(credential, next.run(request)).await
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Go `homeDispatchHeaders` and `homeQueryCredentialFromContext`: `?key=` (else
    /// `?auth_token=`) reaches Home as X-Goog-Api-Key unless a credential header was
    /// sent; outside a request nothing is added. Covers Go
    /// `TestHomeDispatchHeadersAddsQueryKeyCredential`,
    /// `TestHomeDispatchHeadersAddsQueryCredentialFromAccessMetadata` (the query-key
    /// caller), `TestHomeDispatchHeadersKeepsExistingCredentialHeader` and
    /// `TestHomeDispatchHeadersIgnoresHeaderCredentialSource` (the header caller).
    #[tokio::test]
    async fn query_credentials_reach_home_as_x_goog_api_key() {
        let headers = |pairs: &[(&str, &str)]| {
            let mut map = axum::http::HeaderMap::new();
            for (name, value) in pairs {
                map.insert(
                    axum::http::HeaderName::from_bytes(name.as_bytes()).unwrap(),
                    value.parse().unwrap(),
                );
            }
            map
        };
        let goog = |query: &str, pairs: &[(&str, &str)]| {
            let map = headers(pairs);
            let credential = query_credential(query);
            async move {
                QUERY_CREDENTIAL
                    .scope(credential, async {
                        home_headers(&map, None)
                            .into_iter()
                            .filter(|(name, _)| name == "x-goog-api-key")
                            .map(|(_, value)| value)
                            .collect::<Vec<_>>()
                    })
                    .await
            }
        };
        assert_eq!(goog("key=k1&alt=sse", &[]).await, ["k1"]);
        assert_eq!(goog("auth_token=t1", &[("user-agent", "x")]).await, ["t1"]);
        assert_eq!(goog("auth_token=t1&key=%20k2%20", &[]).await, ["k2"]);
        assert!(goog("key=k1", &[("authorization", "Bearer other")]).await.is_empty());
        assert_eq!(
            goog("key=k1", &[("x-goog-api-key", "header-key")]).await,
            ["header-key"]
        );
        assert!(goog("key=k1", &[("x-api-key", "k")]).await.is_empty());
        assert!(goog("alt=sse", &[]).await.is_empty());
        assert!(home_headers(&headers(&[]), None).is_empty());
        // Outside a request, a query-key caller still identifies itself.
        let caller = cpa_core::exec::Caller {
            principal: "local-key".into(),
            source: "query-key",
        };
        let outside: Vec<_> = home_headers(&headers(&[]), Some(&caller));
        assert_eq!(outside, [("x-goog-api-key".to_owned(), "local-key".to_owned())]);
        let header_caller = cpa_core::exec::Caller {
            principal: "local-key".into(),
            source: "authorization",
        };
        assert!(home_headers(&headers(&[]), Some(&header_caller)).is_empty());
    }
}
