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

/// `Auth.AuthKind`: explicit kind, then an API key attribute, then OAuth token metadata.
fn auth_kind(c: &Credential) -> Option<&'static str> {
    let normalize = |s: &str| match s.trim().to_ascii_lowercase().as_str() {
        "apikey" | "api_key" | "api-key" => Some("apikey"),
        "oauth" | "oauth2" => Some("oauth"),
        _ => None,
    };
    if let Some(kind) = c.attributes.get("auth_kind").and_then(|s| normalize(s)) {
        return Some(kind);
    }
    if let Some(kind) = c.str("auth_kind").and_then(normalize) {
        return Some(kind);
    }
    if c.attributes.get("api_key").is_some_and(|k| !k.trim().is_empty()) {
        return Some("apikey");
    }
    let oauth = [
        "access_token",
        "refresh_token",
        "id_token",
        "email",
        "token_type",
        "expires_at",
        "expired",
    ]
    .iter()
    .any(|k| c.str(k).is_some_and(|v| !v.trim().is_empty()))
        || c.metadata
            .get("token")
            .and_then(|t| t.as_object())
            .is_some_and(|t| !t.is_empty());
    oauth.then_some("oauth")
}

/// `credentialPolicyAllows(codex_alpha_search_v1, auth)`.
fn allowed(c: &Credential) -> bool {
    c.provider.eq_ignore_ascii_case("codex")
        && match auth_kind(c) {
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
    // `json.Unmarshal` into {id, model}: each string field independently, else empty.
    #[derive(serde::Deserialize, Default)]
    struct Routing {
        #[serde(default)]
        id: Option<serde_json::Value>,
        #[serde(default)]
        model: Option<serde_json::Value>,
    }
    let routing: Routing = serde_json::from_slice(&body).unwrap_or_default();
    let field = |v: &Option<serde_json::Value>| {
        v.as_ref()
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .trim()
            .to_owned()
    };
    let (id, model) = (field(&routing.id), field(&routing.model));
    let (cfg, policy) = rt.request_snapshot();
    // The credential policy narrows selection; disallowed Codex credentials are excluded.
    let exclude = rt
        .store()
        .snapshot()
        .iter()
        .filter(|c| c.provider == "codex" && !allowed(c))
        .map(|c| c.id.clone())
        .collect();
    let selection = Selection {
        provider: "codex".into(),
        model: model.clone(),
        session: (!id.is_empty()).then_some(id),
        exclude,
        ..Selection::default()
    };
    // ponytail: one selection, no failover or outcome recording, as in Go; plugin model
    // routing and Home dispatch are not ported. Selection error texts follow claude.rs.
    let lease = match rt.acquire_with_policy(selection, &cfg, policy).await {
        Ok(lease) => lease,
        Err(AcquireError::NoCredential) => return error(503, "auth_not_found: no auth available"),
        Err(AcquireError::Cooldown { wait }) => {
            let mut response = error(
                429,
                &format!(
                    "All credentials for model {} are cooling down via provider codex",
                    if model.is_empty() { "requested model" } else { &model }
                ),
            );
            let seconds = wait.as_secs() + u64::from(wait.subsec_nanos() > 0);
            response
                .headers_mut()
                .insert(header::RETRY_AFTER, HeaderValue::from(seconds));
            return response;
        }
        Err(AcquireError::Prepare { error: e, .. }) => {
            return error(e.status, &String::from_utf8_lossy(&e.body));
        }
    };
    let result = rt
        .executors
        .codex
        .alpha_search(&lease.credential, &body, &headers, &lease.execution_model)
        .await;
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
