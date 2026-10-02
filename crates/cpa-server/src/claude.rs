//! Anthropic Messages routes served by Claude credentials.

use std::sync::Arc;

use axum::body::{Body, Bytes};
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use cpa_exec::claude::Endpoint;

use crate::AppState;

/// Upstream response headers worth returning to the client.
const FORWARDED_HEADERS: &[&str] = &["content-type", "request-id"];

pub async fn messages(State(state): State<Arc<AppState>>, body: Bytes) -> Response {
    forward(&state, Endpoint::Messages, body).await
}

pub async fn count_tokens(State(state): State<Arc<AppState>>, body: Bytes) -> Response {
    forward(&state, Endpoint::CountTokens, body).await
}

async fn forward(state: &AppState, endpoint: Endpoint, body: Bytes) -> Response {
    let Some(cred) = state.pick_claude() else {
        return error(StatusCode::SERVICE_UNAVAILABLE, "no Claude credentials loaded");
    };
    let upstream = match state.claude_exec.send(&cred.access_token, endpoint, body).await {
        Ok(upstream) => upstream,
        Err(e) => {
            tracing::warn!(credential = %cred.id, error = %e, "claude upstream request failed");
            return error(StatusCode::BAD_GATEWAY, "upstream request failed");
        }
    };
    let mut response = Response::builder().status(upstream.status().as_u16());
    for &name in FORWARDED_HEADERS {
        if let Some(value) = upstream.headers().get(name) {
            response = response.header(name, value.as_bytes());
        }
    }
    response
        .body(Body::from_stream(upstream.bytes_stream()))
        .unwrap_or_else(|_| error(StatusCode::BAD_GATEWAY, "invalid upstream response headers"))
}

fn error(status: StatusCode, message: &str) -> Response {
    let body = serde_json::json!({
        "type": "error",
        "error": { "type": "api_error", "message": message },
    });
    (status, Json(body)).into_response()
}

// ponytail: static list. The model registry port (internal/registry: embedded catalog,
// remote refresh, per-credential exclusions, aliases) replaces this.
const CLAUDE_MODELS: &[&str] = &[
    "claude-fable-5-1",
    "claude-fable-5",
    "claude-opus-5-5",
    "claude-opus-5",
    "claude-sonnet-5-5",
    "claude-sonnet-5",
    "claude-haiku-4-5-20251001",
];

pub async fn models(State(state): State<Arc<AppState>>) -> Json<serde_json::Value> {
    let data: Vec<_> = if state.claude_credentials().is_empty() {
        Vec::new()
    } else {
        CLAUDE_MODELS
            .iter()
            .map(|id| serde_json::json!({ "id": id, "object": "model", "owned_by": "anthropic" }))
            .collect()
    };
    Json(serde_json::json!({ "object": "list", "data": data }))
}
