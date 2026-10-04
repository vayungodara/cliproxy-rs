//! Codex Alpha Search (`Server.codexAlphaSearch`, internal/api/server_routes.go):
//! `POST /v1/alpha/search` and `POST /backend-api/codex/alpha/search`.
//!
//! The body is already in Codex search format and is never translated. Only OAuth Codex
//! credentials and API keys with `alpha-search: true` may serve it (credential policy
//! `codex_alpha_search_v1`). The upstream status and body reach the client unchanged.

use std::sync::Arc;

use axum::Router;
use axum::body::Bytes;
use axum::extract::State;
use axum::extract::rejection::BytesRejection;
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use cpa_core::credential::Credential;

use crate::runtime::{AcquireError, Runtime, Selection};

pub fn routes() -> Router<Arc<Runtime>> {
    Router::new()
        .route("/v1/alpha/search", post(alpha_search))
        .route("/backend-api/codex/alpha/search", post(alpha_search))
}

/// Go reads at most 16 MiB of the request body (`io.LimitReader`).
const MAX_BODY: usize = 16 << 20;

fn error(status: u16, message: &str) -> Response {
    let status = StatusCode::from_u16(status).unwrap_or(StatusCode::SERVICE_UNAVAILABLE);
    (status, axum::Json(serde_json::json!({ "error": message }))).into_response()
}

/// `json.Unmarshal(body, &struct{ ID, Model string })`, trimmed: keys match
/// case-insensitively and the last duplicate wins; a null or non-string value leaves the
/// field as it was; invalid JSON or a non-object sets nothing.
fn routing(body: &[u8]) -> (String, String) {
    let (mut id, mut model) = (String::new(), String::new());
    let text = String::from_utf8_lossy(body);
    if !gjson::valid(&text) {
        return (id, model);
    }
    let root = gjson::parse(&text);
    if root.kind() == gjson::Kind::Object {
        root.each(|key, value| {
            let slot = match key.str() {
                k if k.eq_ignore_ascii_case("id") => &mut id,
                k if k.eq_ignore_ascii_case("model") => &mut model,
                _ => return true,
            };
            if value.kind() == gjson::Kind::String {
                *slot = value.str().to_owned();
            }
            true
        });
    }
    (id.trim().to_owned(), model.trim().to_owned())
}

fn with_retry_after(mut response: Response, seconds: Option<u64>) -> Response {
    if let Some(seconds) = seconds {
        response
            .headers_mut()
            .insert(header::RETRY_AFTER, HeaderValue::from(seconds));
    }
    response
}

/// `credentialPolicyAllows(codex_alpha_search_v1, auth)`.
fn allowed(c: &Credential) -> bool {
    c.provider.eq_ignore_ascii_case("codex")
        && match cpa_core::registry::dynamic::auth_kind(c) {
            Some("oauth") => true,
            Some("apikey") => c
                .attributes
                .get("codex_alpha_search")
                .is_some_and(|v| v.trim().eq_ignore_ascii_case("true")),
            _ => false,
        }
}

async fn alpha_search(
    State(rt): State<Arc<Runtime>>,
    headers: HeaderMap,
    body: Result<Bytes, BytesRejection>,
) -> Response {
    let body = match body {
        Ok(body) => body.slice(..body.len().min(MAX_BODY)),
        Err(_) => return error(400, "Failed to read search request"),
    };
    let (id, model) = routing(&body);
    let capture = crate::request_logging::current()
        .map(|log| log.capture_sink())
        .unwrap_or_default();
    let (cfg, policy) = rt.request_snapshot();
    // The credential policy narrows selection; disallowed Codex credentials are excluded.
    let exclude = rt
        .store()
        .snapshot()
        .iter()
        .filter(|c| c.provider == "codex" && !allowed(c))
        .map(|c| c.id.clone())
        .collect();
    // Go selects with the route model (`pickNextLegacy`: registry admission and per-model
    // cooldown) and keys session affinity on the explicit session, with `X-Session-ID`
    // set from the body's `id` and no derived identity.
    let mut selection_headers = headers.clone();
    if let Ok(value) = HeaderValue::from_str(&id)
        && !id.is_empty()
    {
        selection_headers.insert("x-session-id", value);
    }
    let meta = cpa_common::session::Meta {
        execution_session: None,
        derived: None,
    };
    let (mut primary, mut parent, fork) = cpa_common::session::explicit_session_ids(&selection_headers, &body, &meta);
    if primary.is_empty() {
        (primary, parent) = cpa_common::session::session_ids(&selection_headers, &body, &meta);
    }
    // Go's Alpha Search context holds only the request ID and this session hierarchy
    // (it never passes `GetContextWithCancel`); Home's canonical session replaces it.
    let client = crate::usage_record::Client {
        session_id: primary.clone(),
        parent_session_id: if parent == primary {
            String::new()
        } else {
            parent.clone()
        },
        request_id: crate::observability::current_request_id().unwrap_or_default(),
        ..Default::default()
    };
    let bound = |s: String| (!s.is_empty()).then(|| cpa_common::session::bound_session_identity(&s));
    let selection = Selection {
        provider: "codex".into(),
        model: model.clone(),
        session: bound(primary),
        session_parent: bound(parent),
        session_fork: fork,
        exclude,
        ..Selection::default()
    };
    // ponytail: one selection, no failover or outcome recording, as in Go; plugin model
    // routing is not ported.
    if let Some(remote) = rt.remote_dispatch() {
        let lease = match home_lease(&rt, remote.as_ref(), selection, &headers).await {
            Ok(lease) => lease,
            Err(response) => return *response,
        };
        let facts = crate::usage_record::Facts {
            client,
            format: cpa_core::format::Format::OpenAIResponse,
            alias: String::new(),
            reasoning_effort: String::new(),
            service_tier: "default".into(),
            generate: false,
            stream: false,
        };
        return search(&rt, lease, &body, &headers, &model, &cfg, &capture, Some(&facts)).await;
    }
    let lease = match rt.acquire(selection, &cfg, policy, &rt.registry()).await {
        Ok(lease) => lease,
        // Go answers `gin.H{"error": err.Error()}` with the selector's error text.
        Err(AcquireError::Unavailable { retry_after: None, .. }) => {
            return error(503, "auth_not_found: no auth available");
        }
        Err(AcquireError::Unavailable {
            retry_after: Some(wait),
            cause,
        }) => {
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
                model: model.clone(),
                cause: None,
                retry_after: Some(wait),
            };
            return with_retry_after(error(503, &text), failure.retry_after());
        }
        Err(AcquireError::Cooldown { wait, cause }) => {
            // `modelCooldownError`: the same JSON the inference routes return, as a string.
            let failure = crate::dispatch::Failure::Cooldown {
                model: model.clone(),
                provider: "codex".into(),
                wait,
                cause,
            };
            return with_retry_after(error(429, &failure.text()), failure.retry_after());
        }
        Err(AcquireError::Prepare { error: e, .. }) => {
            return error(e.status, &String::from_utf8_lossy(&e.body));
        }
    };
    search(&rt, lease, &body, &headers, &model, &cfg, &capture, None).await
}

/// Go `SelectHomeAuthWithCredentialPolicy`: Home picks under `codex_alpha_search_v1`;
/// a pick the policy does not allow ends and is excluded from the next one.
async fn home_lease(
    rt: &Arc<Runtime>,
    remote: &dyn crate::remote::RemoteDispatch,
    selection: Selection,
    headers: &HeaderMap,
) -> Result<crate::runtime::Lease, Box<Response>> {
    let request = crate::remote::RemoteRequest {
        model: selection.model.clone(),
        session_id: selection.session.clone().unwrap_or_default(),
        parent_session_id: selection.session_parent.clone().unwrap_or_default(),
        headers: crate::remote::home_headers(headers, None),
        count: 1,
        retry_round: 0,
        excluded: Vec::new(),
        pinned: String::new(),
        request_id: crate::observability::current_request_id().unwrap_or_default(),
        kind: "http",
        credential_policy: "codex_alpha_search_v1".into(),
    };
    let accept = |c: &Credential| crate::remote::selection_provider(c) == "codex" && allowed(c);
    crate::remote::select(rt, remote, selection, request, accept)
        .await
        .map_err(|refused| {
            Box::new(with_retry_after(
                error(refused.status, &refused.text),
                refused.retry_after,
            ))
        })
}

/// One Alpha Search on `lease`'s credential. A Home pick reports an upstream 401 as a
/// Home result with `unauthorized` (Go `ReportHomeUnauthorized`).
#[allow(clippy::too_many_arguments)]
async fn search(
    rt: &Arc<Runtime>,
    lease: crate::runtime::Lease,
    body: &Bytes,
    headers: &HeaderMap,
    model: &str,
    cfg: &cpa_core::config::Config,
    capture: &cpa_core::exec::CaptureSink,
    unauthorized: Option<&crate::usage_record::Facts>,
) -> Response {
    let model = model.to_owned();
    // `ResolveExecutionModel`: Home's upstream model for a dispatched credential, else the
    // first credential-resolved candidate, else the route model.
    let aliases = cpa_core::registry::dynamic::global_aliases(cfg);
    let execution_model = lease
        .credential
        .attributes
        .get(crate::remote::UPSTREAM_MODEL)
        .map(|m| m.trim().to_owned())
        .filter(|m| !m.is_empty())
        .unwrap_or_else(|| {
            cpa_core::registry::dynamic::execution_models(&aliases, &lease.credential, &model)
                .0
                .into_iter()
                .next()
                .unwrap_or_else(|| model.clone())
        });
    // Go runs a Home pick on the selection's attempt context: a draining dispatcher
    // cancels it before or during the upstream call (`context.Canceled`, status 499).
    let search = rt
        .executors
        .codex
        .alpha_search(&lease.credential, body, headers, &execution_model, cfg, capture);
    let result = match lease.remote_cancelled() {
        None => search.await,
        Some(mut cancelled) => {
            if futures_util::FutureExt::now_or_never(&mut cancelled).is_some() {
                return error(crate::remote::CLIENT_CLOSED, "context canceled");
            }
            tokio::select! {
                biased;
                _ = cancelled => {
                    let text = match rt.executors.codex.alpha_search_url(&lease.credential, cfg) {
                        Some(url) => crate::remote::cancelled_request("POST", &url),
                        None => "context canceled".to_owned(),
                    };
                    return error(crate::remote::CLIENT_CLOSED, &text);
                }
                result = search => result,
            }
        }
    };
    if let (Ok(response), Some(facts)) = (&result, unauthorized)
        && response.status == 401
    {
        let body = match &response.body {
            cpa_core::exec::ResponseBody::Buffered(bytes) => String::from_utf8_lossy(bytes).into_owned(),
            _ => String::new(),
        };
        crate::usage_record::publish_home_unauthorized(rt, facts, &lease.credential, "codex", &model, &body);
    }
    // Dropping the lease reports `Cancelled`: Go's Alpha Search never marks a result.
    drop(lease);
    match result {
        Ok(response) => {
            let status = StatusCode::from_u16(response.status).unwrap_or(StatusCode::BAD_GATEWAY);
            let cpa_core::exec::ResponseBody::Buffered(bytes) = response.body else {
                return error(502, "Failed to read Codex search response");
            };
            let mut out = (status, bytes).into_response();
            if let Some(content_type) = response.headers.get(header::CONTENT_TYPE) {
                out.headers_mut().insert(header::CONTENT_TYPE, content_type.clone());
            }
            out
        }
        Err(e) => error(e.status, &String::from_utf8_lossy(&e.body)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn cred(meta: serde_json::Value, attrs: &[(&str, &str)]) -> Credential {
        let mut c = Credential::from_file(
            Path::new("/a"),
            Path::new("/a/c.json"),
            meta.as_object().unwrap().clone(),
        )
        .unwrap();
        for (k, v) in attrs {
            c.attributes.insert((*k).into(), (*v).into());
        }
        c
    }

    /// Go `encoding/json` into `struct{ ID, Model string }`.
    #[test]
    fn routing_fields_decode_like_go() {
        let r = |s: &str| routing(s.as_bytes());
        let pair = |a: &str, b: &str| (a.to_owned(), b.to_owned());
        assert_eq!(
            r(r#"{"id":"s","id":"t","model":"unregistered","query":"q"}"#),
            pair("t", "unregistered"),
            "duplicates: last wins, other fields kept"
        );
        assert_eq!(r(r#"{"ID":" a ","Model":"m"}"#), pair("a", "m"), "keys fold case");
        assert_eq!(
            r(r#"{"id":1,"model":"m","id":null}"#),
            pair("", "m"),
            "type errors skip only that field"
        );
        assert_eq!(r(r#"{"id":"a","model":"m""#), pair("", ""), "invalid JSON sets nothing");
        assert_eq!(r("[1]"), pair("", ""));
    }

    #[test]
    fn credential_policy_matches_go() {
        let oauth = cred(serde_json::json!({"type":"codex","access_token":"t"}), &[]);
        assert!(allowed(&oauth));
        let key = cred(serde_json::json!({"type":"codex"}), &[("api_key", "k")]);
        assert!(!allowed(&key), "API keys need alpha-search");
        let key = cred(
            serde_json::json!({"type":"codex"}),
            &[("api_key", "k"), ("codex_alpha_search", "true")],
        );
        assert!(allowed(&key));
        let unknown = cred(serde_json::json!({"type":"codex"}), &[]);
        assert!(!allowed(&unknown), "no kind, no access");
        let other = cred(serde_json::json!({"type":"claude","access_token":"t"}), &[]);
        assert!(!allowed(&other));
        let explicit = cred(
            serde_json::json!({"type":"codex","access_token":"t"}),
            &[("auth_kind", "api-key")],
        );
        assert!(!allowed(&explicit), "explicit kind wins over token metadata");
    }
}
