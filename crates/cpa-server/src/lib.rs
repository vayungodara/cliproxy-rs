//! HTTP surface: routes, client-key auth and credential selection.

mod access;
mod claude;

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use axum::routing::{get, post};
use axum::{Json, Router, middleware};
use cpa_core::auth::{self, ClaudeCredential};
use cpa_core::config::Config;
use cpa_exec::claude::ClaudeExecutor;

pub struct AppState {
    pub config: Config,
    claude: Vec<ClaudeCredential>,
    next_claude: AtomicUsize,
    claude_exec: ClaudeExecutor,
}

impl AppState {
    /// Loads credentials from `config.auth_dir`. `claude_base_url` exists so tests can
    /// point the executor at a local server.
    pub fn load(config: Config, claude_base_url: &str) -> anyhow::Result<Self> {
        let claude: Vec<_> = auth::load_dir(&config.auth_dir)?
            .iter()
            .filter_map(ClaudeCredential::from_file)
            .collect();
        Ok(Self {
            config,
            claude,
            next_claude: AtomicUsize::new(0),
            claude_exec: ClaudeExecutor::new(claude_base_url)?,
        })
    }

    pub fn claude_credentials(&self) -> &[ClaudeCredential] {
        &self.claude
    }

    // ponytail: plain round-robin. Priority, weights, fill-first, session affinity and
    // cooldown are the M4 scheduler port (sdk/cliproxy/auth).
    fn pick_claude(&self) -> Option<&ClaudeCredential> {
        if self.claude.is_empty() {
            return None;
        }
        let i = self.next_claude.fetch_add(1, Ordering::Relaxed);
        Some(&self.claude[i % self.claude.len()])
    }
}

pub fn router(state: Arc<AppState>) -> Router {
    let api = Router::new()
        .route("/v1/messages", post(claude::messages))
        .route("/v1/messages/count_tokens", post(claude::count_tokens))
        .route("/v1/models", get(claude::models))
        .layer(middleware::from_fn_with_state(state.clone(), access::require_client_key));
    Router::new()
        .route("/healthz", get(|| async { Json(serde_json::json!({"status": "ok"})) }))
        .merge(api)
        .with_state(state)
}
