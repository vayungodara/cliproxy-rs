//! Realtime and Live: OpenAI Realtime over the ChatGPT/Codex OAuth upstream
//! (internal/client/codex/live, realtime middleware in internal/api/server_middleware.go).
//!
//! - `POST /v1/live`, `/v1/realtime`, `/v1/realtime/calls`: WebRTC SDP negotiation with the
//!   Codex realtime calls endpoint ([`http::call`]). A successful call is remembered
//!   for an hour with its credential, so the sideband and hangup reuse it ([`calls`]).
//! - `GET /v1/live/{id}`, `/v1/realtime/calls/{id}`, `/v1/realtime?call_id=`: the call's
//!   sideband WebSocket; `GET /v1/realtime` without a call: a standard Realtime
//!   WebSocket ([`socket`]).
//! - `POST /v1/realtime/client_secrets` and the legacy `/v1/realtime/sessions`: local
//!   ephemeral keys (`ek_...`) scoped to one session config ([`secrets`]).
//! - Hangup is forwarded; transcription, translation and SIP control are 501 stubs.
//!
//! Three access modes, as Go registers them: `/v1/live*` sits in the ordinary `/v1`
//! group (plain `{"error": msg}`), realtime call and sideband routes also accept
//! ephemeral keys, and the rest take configured keys only with OpenAI-style errors.
//!
//! In Home mode every selection is a Home pick (Go `SelectHomeAuthByKind`); a stored call
//! keeps its pick until the call ends, and its sideband and hangup run on it.
//!
//! Request logs capture the live call and hangup upstream exchanges at Go's
//! `helps.Record*` call sites. The sockets are GET requests, which Go never logs.
//!
//! ponytail: usage logging of live exchanges is not ported. Go also records a failed
//! downstream write of the call's answer; the text is the socket's own error, which
//! `TrackedBody` cannot see.

mod calls;
#[cfg(test)]
mod capture_tests;
#[cfg(feature = "media-relay")]
mod dialer;
#[cfg(test)]
mod home_tests;
mod http;
#[cfg(feature = "media-relay")]
mod media;
mod relay;
mod secrets;
mod socket;
#[cfg(feature = "media-relay")]
mod tunnel;

use std::sync::Arc;

use axum::Router;
use axum::extract::{Request, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use cpa_core::config::Config;
use cpa_core::credential::Credential;

use crate::runtime::{AcquireError, Lease, Runtime, Selection};

/// Realtime state shared by every route: remembered calls and ephemeral keys.
pub(crate) struct Live {
    calls: Arc<calls::Calls>,
    secrets: secrets::Secrets,
    relays: relay::Relays,
}

impl Default for Live {
    fn default() -> Self {
        Self {
            calls: calls::Calls::new(),
            secrets: secrets::Secrets::default(),
            relays: relay::Relays::default(),
        }
    }
}

impl Drop for Live {
    /// Go `Handler.Close` on server shutdown.
    fn drop(&mut self) {
        self.calls.close_all("server_stopped");
    }
}

pub fn routes(rt: &Arc<Runtime>) -> Router<Arc<Runtime>> {
    routes_with(rt, Arc::new(Live::default()))
}

fn routes_with(rt: &Arc<Runtime>, live: Arc<Live>) -> Router<Arc<Runtime>> {
    let auth = |mode| middleware::from_fn_with_state((rt.clone(), live.clone(), mode), authenticate);
    let ordinary = Router::new()
        .route("/v1/live", post(http::call))
        .route("/v1/live/{call_id}", get(socket::live_sideband))
        .route_layer(middleware::from_fn(socket::normalize_upgrade))
        .route_layer(auth(Mode::Ordinary));
    let realtime = Router::new()
        .route("/v1/realtime", get(socket::realtime).post(http::call))
        .route("/v1/realtime/calls", post(http::call))
        .route("/v1/realtime/calls/{call_id}", get(socket::calls_sideband))
        .route(
            "/v1/realtime/translations",
            get(http::translation).post(http::translation),
        )
        .route_layer(middleware::from_fn(socket::normalize_upgrade))
        .route_layer(auth(Mode::Realtime));
    let standard = Router::new()
        .route("/v1/realtime/client_secrets", post(secrets::create))
        .route("/v1/realtime/sessions", post(secrets::legacy))
        .route("/v1/realtime/transcription_sessions", post(http::transcription))
        .route("/v1/realtime/translations/client_secrets", post(http::translation))
        .route("/v1/realtime/calls/{call_id}/hangup", post(http::hangup))
        .route("/v1/realtime/calls/{call_id}/accept", post(http::sip))
        .route("/v1/realtime/calls/{call_id}/reject", post(http::sip))
        .route("/v1/realtime/calls/{call_id}/refer", post(http::sip))
        .route_layer(auth(Mode::Standard));
    ordinary.merge(realtime).merge(standard).layer(axum::Extension(live))
}

/// Which Go middleware guards a route.
#[derive(Clone, Copy)]
enum Mode {
    /// `AuthMiddleware`: configured keys, `{"error": msg}`.
    Ordinary,
    /// `realtimeStandardAuthMiddleware`: configured keys, OpenAI error envelope.
    Standard,
    /// `realtimeAuthMiddleware`: ephemeral `ek_` keys, else as `Standard`.
    Realtime,
}

/// Who called (Go `userApiKey` / `accessProvider`, plus the ephemeral key's grant).
#[derive(Clone, Default)]
pub(crate) struct Principal {
    pub key: String,
    pub provider: String,
    pub secret: Option<Arc<secrets::Grant>>,
}

impl Principal {
    /// `liveSelectionHeaders`: a local ephemeral key never reaches session affinity.
    fn selection_headers(&self, headers: &HeaderMap) -> HeaderMap {
        let mut headers = headers.clone();
        if self.secret.is_some() {
            headers.remove(header::AUTHORIZATION);
            headers.remove(header::PROXY_AUTHORIZATION);
        }
        headers
    }
}

async fn authenticate(
    State((rt, live, mode)): State<(Arc<Runtime>, Arc<Live>, Mode)>,
    mut req: Request,
    next: Next,
) -> Response {
    let authorization = req
        .headers()
        .get(header::AUTHORIZATION)
        .map(|v| String::from_utf8_lossy(v.as_bytes()).into_owned())
        .unwrap_or_default();
    let token = cpa_exec::codex_live::bearer_token(&authorization);
    let principal = if matches!(mode, Mode::Realtime) && token.starts_with(secrets::PREFIX) {
        let Some(grant) = live.secrets.authenticate(token) else {
            return realtime_error(
                401,
                "Realtime client secret is invalid or expired",
                "invalid_request_error",
                "invalid_realtime_client_secret",
            );
        };
        Principal {
            key: Some(&grant.issuer_key)
                .filter(|k| !k.is_empty())
                .unwrap_or(&grant.principal)
                .clone(),
            provider: Some(&grant.issuer_provider)
                .filter(|p| !p.is_empty())
                .cloned()
                .unwrap_or_else(|| "realtime-client-secret".into()),
            secret: Some(grant),
        }
    } else {
        let (authenticated, access) = crate::plugins::authenticate(&rt, req).await;
        req = authenticated;
        match access {
            // No provider registered: Go sets nothing.
            Ok(crate::plugins::Access::Open) => Principal::default(),
            Ok(crate::plugins::Access::Granted { caller, provider }) => Principal {
                key: caller.principal,
                provider,
                secret: None,
            },
            Err(denied) if matches!(mode, Mode::Ordinary) => {
                let body = crate::gojson::Obj::new().str("error", denied.message).finish();
                return crate::respond::gin_json(denied.status, body);
            }
            Err(denied) if denied.status >= 500 => {
                return realtime_error(
                    denied.status,
                    denied.message,
                    "server_error",
                    "authentication_service_error",
                );
            }
            Err(denied) => {
                return realtime_error(denied.status, denied.message, "authentication_error", "invalid_api_key");
            }
        }
    };
    req.extensions_mut().insert(principal);
    next.run(req).await
}

/// `writeRealtimeError`: gin.H marshals with sorted keys.
fn realtime_error(status: u16, message: &str, kind: &str, code: &str) -> Response {
    let detail = crate::gojson::Obj::new()
        .str("code", code)
        .str("message", message)
        .raw("param", "null")
        .str("type", kind)
        .finish();
    crate::respond::gin_json(status, crate::gojson::Obj::new().raw("error", &detail).finish())
}

/// `writeLiveError`: the OpenAI envelope under `/v1/realtime`, else `{"error": msg}`.
fn live_error(realtime: bool, status: u16, message: &str) -> Response {
    if !realtime {
        return crate::respond::gin_json(status, crate::gojson::Obj::new().str("error", message).finish());
    }
    let kind = match status {
        401 => "authentication_error",
        400..=499 => "invalid_request_error",
        _ => "api_error",
    };
    realtime_error(status, message, kind, "realtime_request_failed")
}

/// gin's recovery after a Go panic: 500 with no body.
fn panic_response() -> Response {
    StatusCode::INTERNAL_SERVER_ERROR.into_response()
}

/// A failed credential selection (`writeSelectionError`): status, Go's error text and
/// the scheduler's `Retry-After`.
struct Rejection {
    status: u16,
    text: String,
    /// Seconds, from the scheduler error (`auth.SafeResponseHeaders`).
    retry_after: Option<u64>,
}

impl Rejection {
    fn render(self, realtime: bool) -> Response {
        let mut response = live_error(realtime, self.status, &self.text);
        if let Some(seconds) = self.retry_after {
            response
                .headers_mut()
                .insert(header::RETRY_AFTER, HeaderValue::from(seconds));
        }
        response
    }
}

/// `selectOAuth`: `SelectAuthByKind(codex, "", oauth)`, optionally pinned to one
/// credential (`PinnedAuthMetadataKey`). Session affinity follows the request's session
/// identity. The lease records no outcome (Go marks none for live traffic). In Home mode
/// it is `SelectHomeAuthByKind(codex, model, oauth)`, a pick of the given `kind`.
#[allow(clippy::too_many_arguments)]
async fn select_oauth(
    rt: &Arc<Runtime>,
    cfg: &Config,
    pinned: Option<&str>,
    headers: &HeaderMap,
    body: &[u8],
    execution_session: Option<&str>,
    model: &str,
    kind: &'static str,
) -> Result<Lease, Rejection> {
    if let Some(remote) = rt.remote_dispatch() {
        let (primary, parent, fork) = session_hierarchy(headers, body, execution_session);
        let bound = |s: String| (!s.is_empty()).then(|| cpa_common::session::bound_session_identity(&s));
        let selection = Selection {
            provider: "codex".into(),
            model: model.trim().to_owned(),
            session: bound(primary),
            session_parent: bound(parent),
            session_fork: fork,
            ..Selection::default()
        };
        let request = crate::remote::RemoteRequest {
            model: selection.model.clone(),
            session_id: selection.session.clone().unwrap_or_default(),
            parent_session_id: selection.session_parent.clone().unwrap_or_default(),
            headers: crate::remote::home_headers(headers, None),
            count: 1,
            retry_round: 0,
            excluded: Vec::new(),
            pinned: pinned.unwrap_or_default().trim().to_owned(),
            request_id: crate::observability::current_request_id().unwrap_or_default(),
            kind,
            credential_policy: String::new(),
        };
        let accept = |c: &Credential| {
            crate::remote::selection_provider(c) == "codex"
                && cpa_core::registry::dynamic::auth_kind(c) == Some("oauth")
        };
        return crate::remote::select(rt, remote.as_ref(), selection, request, accept)
            .await
            .map_err(|refused| Rejection {
                status: refused.status,
                text: refused.text,
                retry_after: refused.retry_after,
            });
    }
    let policy = rt.policy();
    let exclude = rt
        .store()
        .snapshot()
        .iter()
        .filter(|c| c.provider == "codex")
        .filter(|c| cpa_core::registry::dynamic::auth_kind(c) != Some("oauth") || pinned.is_some_and(|id| id != c.id))
        .map(|c| c.id.clone())
        .collect();
    let (primary, parent, fork) = session_hierarchy(headers, body, execution_session);
    let bound = |s: String| (!s.is_empty()).then(|| cpa_common::session::bound_session_identity(&s));
    let selection = Selection {
        provider: "codex".into(),
        session: bound(primary),
        session_parent: bound(parent),
        session_fork: fork,
        exclude,
        ..Selection::default()
    };
    rt.acquire(selection, cfg, policy, &rt.registry())
        .await
        .map_err(|error| match error {
            AcquireError::Unavailable { retry_after: None, .. } => Rejection {
                status: 503,
                text: "auth_not_found: no auth available".into(),
                retry_after: None,
            },
            AcquireError::Unavailable {
                retry_after: Some(wait),
                cause,
            } => {
                // `authUnavailableError`: the earliest retry and the last upstream error.
                let mut text = "auth_unavailable: no auth available".to_owned();
                if let Some(summary) = cause
                    .as_deref()
                    .map(crate::dispatch::upstream_summary)
                    .filter(|s| !s.is_empty() && !text.contains(s.as_str()))
                {
                    text.push_str(&format!(" (last upstream error: {summary})"));
                }
                let failure = crate::dispatch::Failure::Unavailable {
                    code: "auth_unavailable",
                    providers: vec!["codex".into()],
                    model: String::new(),
                    cause: None,
                    retry_after: Some(wait),
                };
                Rejection {
                    status: 503,
                    text,
                    retry_after: failure.retry_after(),
                }
            }
            AcquireError::Cooldown { wait, cause } => {
                let failure = crate::dispatch::Failure::Cooldown {
                    model: String::new(),
                    provider: "codex".into(),
                    wait,
                    cause,
                };
                Rejection {
                    status: 429,
                    text: failure.text(),
                    retry_after: failure.retry_after(),
                }
            }
            AcquireError::Prepare { error, .. } => Rejection {
                status: error.status,
                text: String::from_utf8_lossy(&error.body).into_owned(),
                retry_after: None,
            },
        })
}

/// Go `ReportHomeUnauthorized(ctx, selected, "codex", model, body)` from a live handler,
/// whose context holds only the request ID and the session hierarchy (Home's canonical
/// session replaces it on the record).
fn report_unauthorized(rt: &Runtime, credential: &Credential, model: &str, body: &[u8], session: (String, String)) {
    let (session_id, parent_session_id) = session;
    let facts = crate::usage_record::Facts {
        client: crate::usage_record::Client {
            parent_session_id: if parent_session_id == session_id {
                String::new()
            } else {
                parent_session_id
            },
            session_id,
            request_id: crate::observability::current_request_id().unwrap_or_default(),
            ..Default::default()
        },
        format: cpa_core::format::Format::OpenAIResponse,
        alias: String::new(),
        reasoning_effort: String::new(),
        service_tier: "default".into(),
        generate: false,
        stream: false,
    };
    crate::usage_record::publish_home_unauthorized(
        rt,
        &facts,
        credential,
        "codex",
        model,
        &String::from_utf8_lossy(body),
    );
}

/// Resolves when Home drains `lease`; never for a local lease.
fn drained(lease: Option<&Lease>) -> futures_util::future::BoxFuture<'static, ()> {
    lease
        .and_then(Lease::remote_cancelled)
        .unwrap_or_else(|| Box::pin(std::future::pending()))
}

/// The request's session and its parent (`EnrichContextWithSessionHierarchy`): the
/// explicit identity when there is one, else the derived one; plus the fork flag.
fn session_hierarchy(headers: &HeaderMap, body: &[u8], execution_session: Option<&str>) -> (String, String, bool) {
    let meta = cpa_common::session::Meta {
        execution_session,
        derived: None,
    };
    let (primary, parent, fork) = cpa_common::session::explicit_session_ids(headers, body, &meta);
    if !primary.is_empty() {
        return (primary, parent, fork);
    }
    let (primary, parent) = cpa_common::session::session_ids(headers, body, &meta);
    (primary, parent, fork)
}

/// The request's session and its parent, for records.
fn session_ids(headers: &HeaderMap, body: &[u8]) -> (String, String) {
    let (primary, parent, _) = session_hierarchy(headers, body, None);
    (primary, parent)
}

/// The session a call remembers.
fn session_id(headers: &HeaderMap, body: &[u8]) -> String {
    session_hierarchy(headers, body, None).0
}

/// `$CPA-SESSION-ID` for custom headers: the call's stored session, else the request's.
fn header_session(headers: &HeaderMap, body: &[u8], execution_session: Option<&str>, stored: &str) -> Option<String> {
    if !stored.is_empty() {
        return cpa_common::session::cpa_session_id(Some(stored));
    }
    let meta = cpa_common::session::Meta {
        execution_session,
        derived: None,
    };
    cpa_common::session::cpa_session_id(Some(&cpa_common::session::extract_session_id(headers, body, &meta)))
}

/// Go's `X-CPA-TRACE-ID`, stamped when the credential is selected
/// (`logging.SetGinCPATraceID`): the time, the auth index and the request ID.
fn trace(credential: &Credential) -> Option<HeaderValue> {
    let trace = crate::dispatch::Trace::with_request_id(crate::observability::current_request_id());
    trace.selected(credential);
    trace.id().and_then(|id| HeaderValue::from_str(&id).ok())
}

/// Adds the trace header to a response sent after the selection.
fn with_trace(mut response: Response, trace: &Option<HeaderValue>) -> Response {
    if let Some(value) = trace {
        response.headers_mut().insert("x-cpa-trace-id", value.clone());
    }
    response
}
