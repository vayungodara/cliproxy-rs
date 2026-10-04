//! The plugin host in the server (Go `sdk/cliproxy/service_plugins.go`,
//! `internal/api/server_management.go`): one [`cpa_plugin::Host`] per runtime, synced
//! with every published config by a single worker so the latest config always wins.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, RwLock, Weak};

use axum::body::{Body, Bytes};
use axum::extract::Request;
use axum::http::HeaderMap;
use cpa_core::config::Config;
use cpa_core::exec::Caller;
use tokio::sync::watch;

use crate::Runtime;

pub(crate) mod execution;
pub(crate) mod interceptors;

/// The host plus the config it should converge on.
pub struct PluginRuntime {
    host: cpa_plugin::Host,
    latest: watch::Sender<Option<Arc<Config>>>,
    access: RwLock<Arc<AccessRegistry>>,
}

impl Default for PluginRuntime {
    fn default() -> Self {
        Self::with_host(cpa_plugin::Host::new())
    }
}

impl PluginRuntime {
    /// Wraps a host that is already running (the binary's, which loaded plugins for
    /// their command-line flags before the config was read).
    pub(crate) fn with_host(host: cpa_plugin::Host) -> Self {
        Self {
            host,
            latest: watch::Sender::new(None),
            access: RwLock::default(),
        }
    }

    pub fn host(&self) -> &cpa_plugin::Host {
        &self.host
    }

    /// Records a published config; the worker started by [`start`] applies it.
    pub(crate) fn config_published(&self, cfg: Arc<Config>) {
        self.latest.send_replace(Some(cfg));
    }
}

/// Go's startup and reload sequence (`syncPluginRuntimeConfigForConfig` and
/// `RefreshPluginManagementRoutes`): apply the config, register the client-key and
/// plugin frontend auth providers, then rebuild the plugin management routes around the
/// built-in ones.
/// ponytail: usage, model and executor registration join the sync with the dispatch
/// wiring.
pub async fn sync(rt: &Runtime, cfg: Arc<Config>) {
    let host = rt.plugins();
    host.apply_config(cfg.clone()).await;
    // Go `configaccess.Register` (reload: `ApplyAccessProviders`) runs before the plugin
    // providers register.
    let (providers, exclusive) = host.frontend_auth_providers().await;
    rt.plugin_runtime()
        .register_access(!cfg.api_keys.is_empty(), providers, exclusive);
    host.register_management_routes(&reserved_management_routes()).await;
}

// ---- Frontend auth (Go `sdk/access`) -------------------------------------------------

/// Go `sdkaccess.AccessProviderTypeConfigAPIKey`: the client-key provider's registry key.
const CONFIG_PROVIDER_KEY: &str = "config-api-key";
/// Go `sdkaccess.DefaultAccessProviderName`: the client-key provider's identifier.
pub(crate) const CONFIG_PROVIDER: &str = "config-inline";

/// Go's global access provider registry as the syncs leave it: keys in first-registration
/// order (a re-registered key keeps its place; an unregistered one loses it), the plugin
/// behind each plugin key, and the exclusive key.
#[derive(Default)]
struct AccessRegistry {
    order: Vec<String>,
    plugins: HashMap<String, String>,
    exclusive: Option<String>,
}

enum Provider<'a> {
    ClientKeys,
    Plugin(&'a str),
}

impl AccessRegistry {
    /// Go `RegisteredProviders`. The client-key provider is present exactly when keys are
    /// configured now; before the first sync it sits first, as Go registers it before any
    /// plugin.
    fn providers(&self, client_keys: bool) -> Vec<Provider<'_>> {
        if let Some(id) = self.exclusive.as_ref().and_then(|key| self.plugins.get(key)) {
            return vec![Provider::Plugin(id)];
        }
        let mut out = Vec::with_capacity(self.order.len() + 1);
        if client_keys && !self.order.iter().any(|k| k == CONFIG_PROVIDER_KEY) {
            out.push(Provider::ClientKeys);
        }
        for key in &self.order {
            if key == CONFIG_PROVIDER_KEY {
                if client_keys {
                    out.push(Provider::ClientKeys);
                }
            } else if let Some(id) = self.plugins.get(key) {
                out.push(Provider::Plugin(id));
            }
        }
        out
    }
}

impl PluginRuntime {
    fn access(&self) -> Arc<AccessRegistry> {
        self.access.read().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// Go `configaccess.Register` then `RegisterFrontendAuthProviders`: register or
    /// unregister the client-key provider, register each plugin provider, set the
    /// exclusive key, and drop plugin keys no longer offered.
    fn register_access(&self, client_keys: bool, providers: Vec<(String, String)>, exclusive: Option<String>) {
        let mut slot = self.access.write().unwrap_or_else(|e| e.into_inner());
        let mut next = AccessRegistry {
            order: slot.order.clone(),
            plugins: HashMap::new(),
            exclusive,
        };
        let register = |order: &mut Vec<String>, key: &str| {
            if !order.iter().any(|k| k == key) {
                order.push(key.to_owned());
            }
        };
        if client_keys {
            register(&mut next.order, CONFIG_PROVIDER_KEY);
        } else {
            next.order.retain(|k| k != CONFIG_PROVIDER_KEY);
        }
        for (key, id) in providers {
            register(&mut next.order, &key);
            next.plugins.insert(key, id);
        }
        next.order
            .retain(|k| k == CONFIG_PROVIDER_KEY || next.plugins.contains_key(k));
        *slot = Arc::new(next);
    }
}

/// A request the access providers let through.
pub(crate) enum Access {
    /// No provider registered: Go's manager returns no result and sets nothing.
    Open,
    /// Go `sdkaccess.Result`: the caller and the provider identifier that accepted it.
    /// ponytail: a plugin's result metadata is not kept (`Caller` has no field for it;
    /// only Go's Home mode reads it).
    Granted { caller: Caller, provider: String },
}

/// Go `sdkaccess.AuthError` as the middlewares answer it.
pub(crate) struct Denied {
    pub status: u16,
    pub message: &'static str,
}

const MISSING_API_KEY: &str = "Missing API key";

/// Go `sdkaccess.Manager.Authenticate` over the registered providers: client keys, then
/// plugin frontend auth providers in registration order (or only the exclusive one).
/// The first acceptance wins; afterwards "Invalid API key" if any provider saw an
/// invalid key, else "Missing API key". The request comes back with its body intact.
pub(crate) async fn authenticate(rt: &Runtime, req: Request) -> (Request, Result<Access, Denied>) {
    let config = rt.config();
    let registry = rt.plugin_runtime().access();
    let providers = registry.providers(!config.api_keys.is_empty());
    if providers.is_empty() {
        return (req, Ok(Access::Open));
    }
    let (parts, mut body) = req.into_parts();
    let mut read: Option<Bytes> = None;
    let mut invalid = false;
    let mut outcome = None;
    for provider in providers {
        match provider {
            Provider::ClientKeys => {
                match crate::access::authenticate(
                    &config.api_keys,
                    &parts.headers,
                    parts.uri.query().unwrap_or_default(),
                ) {
                    Ok(caller) => {
                        outcome = Some(Ok(Access::Granted {
                            caller,
                            provider: CONFIG_PROVIDER.to_owned(),
                        }));
                        break;
                    }
                    Err(message) => invalid |= message != MISSING_API_KEY,
                }
            }
            Provider::Plugin(id) => {
                // Go `readAndRestoreRequestBody`: the whole body, once. Go reads it
                // without a bound; here it stops at MAX_REQUEST_BYTES, as the routes do.
                let bytes = match &read {
                    Some(bytes) => bytes.clone(),
                    None => match read_limited(std::mem::take(&mut body)).await {
                        Ok(Some(bytes)) => read.insert(bytes).clone(),
                        Ok(None) => {
                            outcome = Some(Err(Denied {
                                status: 413,
                                message: "request body too large",
                            }));
                            break;
                        }
                        Err(error) => {
                            tracing::error!(
                                "authentication middleware error: failed to read plugin auth request body: {error}"
                            );
                            outcome = Some(Err(Denied {
                                status: 500,
                                message: "failed to read plugin auth request body",
                            }));
                            break;
                        }
                    },
                };
                let request = cpa_plugin::api::FrontendAuthRequest {
                    method: parts.method.as_str().to_owned(),
                    path: crate::management::percent_decode(parts.uri.path()),
                    headers: go_request_header(&parts.headers),
                    query: go_values(parts.uri.query().unwrap_or_default()),
                    body: cpa_plugin::gojson::NonNilBytes(bytes),
                };
                if let Some(accepted) = rt.plugins().frontend_authenticate(id, &request).await {
                    outcome = Some(Ok(Access::Granted {
                        caller: Caller {
                            principal: accepted.principal,
                            source: "",
                        },
                        provider: accepted.provider,
                    }));
                    break;
                }
            }
        }
    }
    let body = match read {
        Some(bytes) => Body::from(bytes),
        None => body,
    };
    let outcome = outcome.unwrap_or(Err(Denied {
        status: 401,
        message: if invalid { "Invalid API key" } else { MISSING_API_KEY },
    }));
    (Request::from_parts(parts, body), outcome)
}

/// The whole body, or `None` once it passes [`crate::MAX_REQUEST_BYTES`].
async fn read_limited(body: Body) -> Result<Option<Bytes>, axum::Error> {
    use futures_util::StreamExt as _;
    let mut stream = body.into_data_stream();
    let mut out = bytes::BytesMut::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk?;
        if out.len() + chunk.len() > crate::MAX_REQUEST_BYTES {
            return Ok(None);
        }
        out.extend_from_slice(&chunk);
    }
    Ok(Some(out.freeze()))
}

/// A server request's `http.Header` as Go's handlers see it: canonical names, values in
/// order, and no `Host` (Go moves it to `Request.Host`). A chunked request also loses
/// `Transfer-Encoding`, `Content-Length` and `Trailer` (Go `readTransfer`).
pub(crate) fn go_request_header(headers: &HeaderMap) -> cpa_plugin::gojson::Header {
    let chunked = headers.contains_key(axum::http::header::TRANSFER_ENCODING);
    let mut out = cpa_plugin::gojson::Header::new();
    for (name, value) in headers {
        let dropped = name == axum::http::header::HOST
            || (chunked
                && (name == axum::http::header::TRANSFER_ENCODING
                    || name == axum::http::header::CONTENT_LENGTH
                    || name == axum::http::header::TRAILER));
        if dropped {
            continue;
        }
        out.entry(cpa_exec::proxy::canonical_header(name.as_str()))
            .or_default()
            .push(String::from_utf8_lossy(value.as_bytes()).into_owned());
    }
    out
}

/// `Request.URL.Query()` as `url.Values`.
pub(crate) fn go_values(raw_query: &str) -> cpa_plugin::gojson::Header {
    let mut out = cpa_plugin::gojson::Header::new();
    for (k, v) in crate::management::go_query(raw_query) {
        out.entry(k).or_default().push(v);
    }
    out
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

#[cfg(test)]
mod tests {
    use super::*;

    fn outcome(result: &Result<Access, Denied>) -> (u16, &'static str) {
        match result {
            Ok(_) => (200, ""),
            Err(denied) => (denied.status, denied.message),
        }
    }

    /// A plugin frontend auth provider reads the body even after the client keys
    /// rejected the request, so the read stops at MAX_REQUEST_BYTES.
    #[tokio::test]
    async fn plugin_auth_reads_a_bounded_body() {
        let dir = cpa_plugin::testing::scratch(&std::env::temp_dir(), "plugin-auth-body");
        let cfg = Config::parse(&format!(
            "config-version: 8\nauth-dir: {}\naccess:\n  api-keys: [client-key]\n",
            dir.display()
        ))
        .unwrap();
        let rt = crate::testing::runtime(
            cfg,
            Vec::new(),
            cpa_exec::Executors {
                claude: cpa_exec::claude::ClaudeExecutor::new("http://127.0.0.1:1").unwrap(),
                codex: Default::default(),
                devices: Default::default(),
                openai: Default::default(),
                google: Default::default(),
            },
        );
        // A provider for a plugin that is not loaded: it never accepts.
        rt.plugin_runtime()
            .register_access(true, vec![("plugin-auth".into(), "p".into())], None);
        let request = |key: Option<&str>, body: Body| {
            let mut builder = Request::builder().method("POST").uri("/v1/chat/completions");
            if let Some(key) = key {
                builder = builder.header("authorization", format!("Bearer {key}"));
            }
            builder.body(body).unwrap()
        };
        for key in [None, Some("wrong")] {
            let big = Body::from(vec![b'x'; crate::MAX_REQUEST_BYTES + 1]);
            let (_, result) = authenticate(&rt, request(key, big)).await;
            assert_eq!(outcome(&result), (413, "request body too large"), "{key:?}");
        }
        // At the limit the body is read, offered and handed back whole.
        let (req, result) = authenticate(
            &rt,
            request(Some("wrong"), Body::from(vec![b'y'; crate::MAX_REQUEST_BYTES])),
        )
        .await;
        assert_eq!(outcome(&result), (401, "Invalid API key"));
        let body = axum::body::to_bytes(req.into_body(), usize::MAX).await.unwrap();
        assert_eq!(body.len(), crate::MAX_REQUEST_BYTES);
        let (_, result) = authenticate(&rt, request(None, Body::from("{}"))).await;
        assert_eq!(outcome(&result), (401, MISSING_API_KEY));
        let (_, result) = authenticate(&rt, request(Some("client-key"), Body::from("{}"))).await;
        assert_eq!(outcome(&result), (200, ""));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
