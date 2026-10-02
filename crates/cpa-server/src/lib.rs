//! HTTP surface: routes and client-key auth over an axum-independent [`runtime`].

mod access;
mod claude;
mod refresh;
pub mod runtime;
pub mod scheduler;

use std::sync::Arc;

use axum::extract::DefaultBodyLimit;
use axum::routing::{get, post};
use axum::{Json, Router, middleware};

pub use runtime::Runtime;

// ponytail: fixed 64 MiB request cap so image-heavy Claude requests pass (axum's default
// is 2 MiB; gin has none). Make it configurable if anyone needs more.
pub const MAX_REQUEST_BYTES: usize = 64 * 1024 * 1024;

pub fn router(rt: Arc<Runtime>) -> Router {
    let api = Router::new()
        .route("/v1/messages", post(claude::messages))
        .route("/v1/messages/count_tokens", post(claude::count_tokens))
        .route("/v1/models", get(claude::models))
        .layer(middleware::from_fn_with_state(rt.clone(), access::require_client_key));
    Router::new()
        .route("/healthz", get(|| async { Json(serde_json::json!({"status": "ok"})) }))
        .merge(api)
        .layer(DefaultBodyLimit::max(MAX_REQUEST_BYTES))
        .with_state(rt)
}
