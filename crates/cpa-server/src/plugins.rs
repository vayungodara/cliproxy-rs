//! The plugin host in the server (Go `sdk/cliproxy/service_plugins.go`,
//! `internal/api/server_management.go`): one [`cpa_plugin::Host`] per runtime, synced
//! with every published config by a single worker so the latest config always wins.

use std::collections::HashSet;
use std::sync::{Arc, Weak};

use cpa_core::config::Config;
use tokio::sync::watch;

use crate::Runtime;

/// The host plus the config it should converge on.
pub struct PluginRuntime {
    host: cpa_plugin::Host,
    latest: watch::Sender<Option<Arc<Config>>>,
}

impl Default for PluginRuntime {
    fn default() -> Self {
        Self {
            host: cpa_plugin::Host::new(),
            latest: watch::Sender::new(None),
        }
    }
}

impl PluginRuntime {
    pub fn host(&self) -> &cpa_plugin::Host {
        &self.host
    }

    /// Records a published config; the worker started by [`start`] applies it.
    pub(crate) fn config_published(&self, cfg: Arc<Config>) {
        self.latest.send_replace(Some(cfg));
    }
}

/// Go's startup and reload sequence (`syncPluginRuntimeConfigForConfig` and
/// `RefreshPluginManagementRoutes`): apply the config, then rebuild the plugin
/// management routes around the built-in ones.
/// ponytail: frontend auth, usage, model and executor registration join the sync with
/// the dispatch wiring.
pub async fn sync(rt: &Runtime, cfg: Arc<Config>) {
    let host = rt.plugins();
    host.apply_config(cfg).await;
    host.register_management_routes(&reserved_management_routes()).await;
}

/// Syncs the current config now, then follows every later publish. Call once at
/// startup, before serving (Go applies plugins before the server starts).
pub async fn start(rt: &Arc<Runtime>) {
    let mut rx = rt.plugin_runtime().latest.subscribe();
    rx.mark_unchanged();
    sync(rt, rt.config()).await;
    let weak: Weak<Runtime> = Arc::downgrade(rt);
    tokio::spawn(async move {
        while rx.changed().await.is_ok() {
            let Some(cfg) = rx.borrow_and_update().clone() else {
                continue;
            };
            let Some(rt) = weak.upgrade() else {
                return;
            };
            sync(&rt, cfg).await;
        }
    });
}

/// Go `registeredManagementRouteKeys`: every built-in `/v0/management` route as gin
/// lists it. Plugins cannot register these keys even where this server does not
/// implement the route, so a plugin keeps the same routes under both servers.
/// ponytail: Go registers these only once a management secret exists; until then it
/// lets plugins claim them. The full list is used unconditionally.
pub fn reserved_management_routes() -> HashSet<String> {
    GO_V0_MANAGEMENT_ROUTES.iter().map(|r| (*r).to_owned()).collect()
}

/// `internal/api/server_management.go` at 6fecc6e.
const GO_V0_MANAGEMENT_ROUTES: [&str; 144] = [
    "DELETE /v0/management/api-keys",
    "DELETE /v0/management/auth-files",
    "DELETE /v0/management/claude-api-key",
    "DELETE /v0/management/codex-api-key",
    "DELETE /v0/management/gemini-api-key",
    "DELETE /v0/management/interactions-api-key",
    "DELETE /v0/management/logs",
    "DELETE /v0/management/meta-api-key",
    "DELETE /v0/management/oauth-excluded-models",
    "DELETE /v0/management/oauth-model-alias",
    "DELETE /v0/management/oauth-request-scoped-errors",
    "DELETE /v0/management/oauth-session",
    "DELETE /v0/management/openai-compatibility",
    "DELETE /v0/management/plugins/:id",
    "DELETE /v0/management/plugins/:id/quota",
    "DELETE /v0/management/proxy-url",
    "DELETE /v0/management/vertex-api-key",
    "DELETE /v0/management/xai-api-key",
    "GET /v0/management/anthropic-auth-url",
    "GET /v0/management/antigravity-auth-url",
    "GET /v0/management/api-key-usage",
    "GET /v0/management/api-keys",
    "GET /v0/management/auth-files",
    "GET /v0/management/auth-files/download",
    "GET /v0/management/auth-files/models",
    "GET /v0/management/claude-api-key",
    "GET /v0/management/codex-api-key",
    "GET /v0/management/codex-auth-url",
    "GET /v0/management/config",
    "GET /v0/management/config.yaml",
    "GET /v0/management/debug",
    "GET /v0/management/devin-auth-url",
    "GET /v0/management/error-logs-max-files",
    "GET /v0/management/force-model-prefix",
    "GET /v0/management/gemini-api-key",
    "GET /v0/management/get-auth-status",
    "GET /v0/management/interactions-api-key",
    "GET /v0/management/kimi-ai-auth-url",
    "GET /v0/management/kimi-auth-url",
    "GET /v0/management/latest-version",
    "GET /v0/management/logging-to-file",
    "GET /v0/management/logs",
    "GET /v0/management/logs-max-total-size-mb",
    "GET /v0/management/max-retry-credentials",
    "GET /v0/management/max-retry-interval",
    "GET /v0/management/meta-api-key",
    "GET /v0/management/meta-auth-url",
    "GET /v0/management/model-definitions/:channel",
    "GET /v0/management/oauth-callback",
    "GET /v0/management/oauth-excluded-models",
    "GET /v0/management/oauth-model-alias",
    "GET /v0/management/oauth-request-scoped-errors",
    "GET /v0/management/openai-compatibility",
    "GET /v0/management/plugin-store",
    "GET /v0/management/plugins",
    "GET /v0/management/plugins/:id/config",
    "GET /v0/management/plugins/:id/quota",
    "GET /v0/management/proxy-url",
    "GET /v0/management/quota-exceeded/switch-preview-model",
    "GET /v0/management/quota-exceeded/switch-project",
    "GET /v0/management/quota/providers",
    "GET /v0/management/request-error-logs",
    "GET /v0/management/request-error-logs/:name",
    "GET /v0/management/request-log",
    "GET /v0/management/request-log-by-id/:id",
    "GET /v0/management/request-retry",
    "GET /v0/management/routing/strategy",
    "GET /v0/management/usage-queue",
    "GET /v0/management/usage-statistics-enabled",
    "GET /v0/management/vertex-api-key",
    "GET /v0/management/ws-auth",
    "GET /v0/management/xai-api-key",
    "GET /v0/management/xai-auth-url",
    "PATCH /v0/management/api-keys",
    "PATCH /v0/management/auth-files/fields",
    "PATCH /v0/management/auth-files/status",
    "PATCH /v0/management/claude-api-key",
    "PATCH /v0/management/codex-api-key",
    "PATCH /v0/management/debug",
    "PATCH /v0/management/error-logs-max-files",
    "PATCH /v0/management/force-model-prefix",
    "PATCH /v0/management/gemini-api-key",
    "PATCH /v0/management/interactions-api-key",
    "PATCH /v0/management/logging-to-file",
    "PATCH /v0/management/logs-max-total-size-mb",
    "PATCH /v0/management/max-retry-credentials",
    "PATCH /v0/management/max-retry-interval",
    "PATCH /v0/management/meta-api-key",
    "PATCH /v0/management/oauth-excluded-models",
    "PATCH /v0/management/oauth-model-alias",
    "PATCH /v0/management/oauth-request-scoped-errors",
    "PATCH /v0/management/openai-compatibility",
    "PATCH /v0/management/plugins/:id/config",
    "PATCH /v0/management/plugins/:id/enabled",
    "PATCH /v0/management/proxy-url",
    "PATCH /v0/management/quota-exceeded/switch-preview-model",
    "PATCH /v0/management/quota-exceeded/switch-project",
    "PATCH /v0/management/request-log",
    "PATCH /v0/management/request-retry",
    "PATCH /v0/management/routing/strategy",
    "PATCH /v0/management/usage-statistics-enabled",
    "PATCH /v0/management/vertex-api-key",
    "PATCH /v0/management/ws-auth",
    "PATCH /v0/management/xai-api-key",
    "POST /v0/management/api-call",
    "POST /v0/management/auth-files",
    "POST /v0/management/auth-files/refresh",
    "POST /v0/management/oauth-callback",
    "POST /v0/management/plugin-store/:id/install",
    "POST /v0/management/plugins/:id/quota",
    "POST /v0/management/plugins/:id/quota/reset",
    "POST /v0/management/quota/fetch",
    "POST /v0/management/quota/reset",
    "POST /v0/management/reset-quota",
    "POST /v0/management/vertex/import",
    "PUT /v0/management/api-keys",
    "PUT /v0/management/claude-api-key",
    "PUT /v0/management/codex-api-key",
    "PUT /v0/management/config.yaml",
    "PUT /v0/management/debug",
    "PUT /v0/management/error-logs-max-files",
    "PUT /v0/management/force-model-prefix",
    "PUT /v0/management/gemini-api-key",
    "PUT /v0/management/interactions-api-key",
    "PUT /v0/management/logging-to-file",
    "PUT /v0/management/logs-max-total-size-mb",
    "PUT /v0/management/max-retry-credentials",
    "PUT /v0/management/max-retry-interval",
    "PUT /v0/management/meta-api-key",
    "PUT /v0/management/oauth-excluded-models",
    "PUT /v0/management/oauth-model-alias",
    "PUT /v0/management/oauth-request-scoped-errors",
    "PUT /v0/management/openai-compatibility",
    "PUT /v0/management/plugins/:id/config",
    "PUT /v0/management/proxy-url",
    "PUT /v0/management/quota-exceeded/switch-preview-model",
    "PUT /v0/management/quota-exceeded/switch-project",
    "PUT /v0/management/request-log",
    "PUT /v0/management/request-retry",
    "PUT /v0/management/routing/strategy",
    "PUT /v0/management/usage-statistics-enabled",
    "PUT /v0/management/vertex-api-key",
    "PUT /v0/management/ws-auth",
    "PUT /v0/management/xai-api-key",
];
