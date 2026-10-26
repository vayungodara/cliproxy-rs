//! Management API (v8, plus the few legacy v0 reads the dashboard predates).
//!
//! Routing follows gin: an unknown path or an unregistered method on a known path is a
//! bare 404 that never reaches authentication. Access rules live in [`access`].
use std::path::PathBuf;
use std::sync::{Arc, Mutex, PoisonError};

use axum::body::Bytes;
use axum::extract::rejection::BytesRejection;
use axum::extract::{DefaultBodyLimit, OriginalUri, State};
use axum::handler::Handler;
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{MethodRouter, any, get};
use axum::{Router, middleware};
use cpa_common::gostr::GoStr;
use cpa_core::config::{Config, ConfigDocument, archive_comments, credentials, is_bcrypt};
use cpa_core::credential::{Credential, Source};
use serde_json::{Value, json};

use crate::Runtime;
use crate::scheduler::{ErrorRule, Policy};

mod access;
mod api_call;
mod auth_files;
mod auth_index;
mod iso_currency;
pub(crate) mod legacy;
mod logs;
pub(crate) mod multipart;
mod oauth;
pub mod observability;
mod plugin_auth;
mod plugin_oauth;
mod plugin_store;
mod plugins;
pub(crate) mod quota;
mod quota_probe;
mod siwc;
pub use access::cors;
pub(crate) use access::{Cors, cors_headers};
pub(crate) use plugins::go_query;

pub struct Management {
    pub(crate) rt: Arc<Runtime>,
    pub(crate) path: PathBuf,
    // ponytail: one lock for config and auth disk operations; split only if management
    // throughput matters. The watcher shares it, preventing stale disk publication.
    pub(crate) disk: Mutex<()>,
    /// Uploaded files no synthesizer claims, kept listed until restart like Go's
    /// fallback auths (see `credentials::upload_fallback`).
    pub(crate) fallbacks: Mutex<std::collections::BTreeSet<PathBuf>>,
    /// Credentials disabled through the status or fields endpoints: Go's in-memory
    /// auth then reports `disabled via management API` until re-enabled.
    pub(crate) disabled_via_api: Mutex<std::collections::BTreeSet<String>>,
    /// When runtime-only credentials were first seen and last changed (Go's
    /// `CreatedAt`/`UpdatedAt` for them; the store keeps no timestamps).
    pub(crate) runtime_seen: Mutex<std::collections::HashMap<String, auth_files::RuntimeSeen>>,
    /// Go net/http clients by proxy for `api-call`.
    pub(crate) clients: cpa_exec::proxy::GoClients,
    pub(crate) latest_release_url: std::borrow::Cow<'static, str>,
    pub(crate) update_check_disabled: bool,
    // No task or expiry timer: allocated only after a successful, explicit check.
    // Wall-clock time, so a cached result also expires across sleep or hibernation.
    pub(crate) latest_release: tokio::sync::Mutex<Option<(std::time::SystemTime, String)>>,
    /// Pending and recent management logins (Go `oauthSessionStore`).
    pub(crate) oauth: oauth::Sessions,
    /// Callback forwarders by port (Go `callbackForwarders`).
    pub(crate) forwarders: Mutex<std::collections::HashMap<u16, oauth::Forwarder>>,
    pub(crate) login_base: Option<String>,
    /// Told the state of every management login that has finished (tests only).
    pub(crate) login_settled: Option<tokio::sync::mpsc::UnboundedSender<String>>,
    /// Go `Handler.logDir`: resolved once at startup.
    pub(crate) log_dir: PathBuf,
    /// The remote store mirroring config and auth files, when one is configured.
    pub(crate) store: Option<Arc<dyn crate::persist::StorePersister>>,
    /// Pins auth-dir across reloads (tests only; see [`Options::auth_dir`]).
    auth_dir: Option<PathBuf>,
    /// The zone log line timestamps are parsed in (Go `time.Local` when `None`).
    pub(crate) log_zone: logs::Zone,
    /// Plugin store test seams and release cache (Go `Handler.pluginStore*`).
    pub(crate) plugin_store: plugin_store::StoreState,
    access: access::Access,
}

#[derive(Default)]
pub struct Options {
    /// `--password`: accepted from loopback clients only, like Go's local password.
    pub local_password: String,
    /// `-tui -standalone`: the local password alone keeps management on, across
    /// reloads, for loopback clients. Go refuses every request without a configured
    /// secret, so its standalone TUI cannot sign in with a keyless config.
    pub standalone: bool,
    /// Overrides the `MANAGEMENT_PASSWORD` environment variable when set.
    pub management_password: Option<String>,
    /// Overrides [`observability::LATEST_RELEASE_URL`] (tests point it at a local server).
    pub latest_release_url: Option<String>,
    /// Overrides CLIPROXY_NO_UPDATE_CHECK (tests only).
    pub update_check_disabled: Option<bool>,
    /// One local base URL for every provider's login endpoints (tests only).
    pub login_base: Option<String>,
    /// Receives the state of every management login once its worker has finished,
    /// saved, failed or cancelled (tests only).
    pub login_settled: Option<tokio::sync::mpsc::UnboundedSender<String>>,
    /// Overrides the log directory Go resolves at startup (tests only).
    pub log_dir: Option<PathBuf>,
    /// A Postgres, git or object store mirroring config and auth files (Go's
    /// registered token store when it persists remotely).
    pub store: Option<Arc<dyn crate::persist::StorePersister>>,
    /// Parses log line timestamps in this zone instead of the local one (tests only).
    pub log_zone: Option<chrono::FixedOffset>,
    /// Scans this directory for credentials on every reload instead of the config's
    /// auth-dir, so a config without one never reads the default `~/.cli-proxy-api`
    /// (tests only). A configured store still wins.
    pub auth_dir: Option<PathBuf>,
    /// The only plugin store registry (Go `pluginStoreRegistryURL`; tests only).
    pub plugin_store_registry_url: Option<String>,
    /// The plugin store's HTTP client (Go `pluginStoreHTTPClient`; tests only).
    pub plugin_store_http: Option<Arc<dyn cpa_plugin::store::Doer>>,
    /// The plugin store's GitHub rate limiter (Go `pluginStoreRateLimiter`); the
    /// process-wide one when unset.
    pub plugin_store_rate_limiter: Option<Arc<cpa_plugin::store::GitHubRateLimiter>>,
}

impl Management {
    /// Go `managementRoutesEnabled`, for the RESP protocol on the main listener.
    pub(crate) fn routes_enabled(&self) -> bool {
        self.access.available()
    }

    /// Go `AuthenticateManagementKey` for callers outside HTTP (the RESP protocol).
    pub(crate) async fn authenticate_key(
        &self,
        ip: &str,
        local: bool,
        provided: &[u8],
    ) -> Result<(), (StatusCode, String)> {
        self.access.authenticate(&self.rt.config(), ip, local, provided).await
    }

    pub fn new(rt: Arc<Runtime>, path: PathBuf) -> Arc<Self> {
        Self::with_options(rt, path, Options::default())
    }

    /// Captures startup-only settings (trusted proxies, environment secret) and
    /// publishes the scheduler policy derived from the current config.
    pub fn with_options(rt: Arc<Runtime>, path: PathBuf, options: Options) -> Arc<Self> {
        let cfg = rt.config();
        rt.publish_policy(policy(&cfg));
        let latest_release_url = options
            .latest_release_url
            .clone()
            .map(std::borrow::Cow::Owned)
            .unwrap_or(std::borrow::Cow::Borrowed(observability::LATEST_RELEASE_URL));
        let update_check_disabled = options
            .update_check_disabled
            .unwrap_or_else(|| std::env::var("CLIPROXY_NO_UPDATE_CHECK").is_ok_and(|v| v == "1"));
        let login_base = options.login_base.clone();
        let log_dir = options
            .log_dir
            .clone()
            .unwrap_or_else(|| crate::logging::resolve_log_dir(&cfg));
        let mut options = options;
        let store = options.store.take();
        let auth_dir = options.auth_dir.take();
        let login_settled = options.login_settled.take();
        let log_zone = options.log_zone;
        let plugin_store = plugin_store::StoreState::new(
            options.plugin_store_registry_url.take(),
            options.plugin_store_http.take(),
            options.plugin_store_rate_limiter.take(),
        );
        let access = access::Access::new(&cfg, options);
        rt.usage_queue().configure(access.available(), &cfg);
        let state = Arc::new(Self {
            access,
            rt,
            path,
            disk: Mutex::new(()),
            fallbacks: Mutex::default(),
            disabled_via_api: Mutex::default(),
            runtime_seen: Mutex::default(),
            clients: cpa_exec::proxy::GoClients::new(cpa_exec::proxy::Hooks::default()),
            latest_release_url,
            update_check_disabled,
            latest_release: tokio::sync::Mutex::new(None),
            oauth: oauth::Sessions::default(),
            forwarders: Mutex::default(),
            login_base,
            login_settled,
            log_dir,
            store,
            auth_dir,
            log_zone,
            plugin_store,
        });
        oauth::install_callback_sink(&state);
        access::start_purge(&state);
        plugin_auth::attach(&state);
        state
    }

    /// Go `Watcher.mirroredAuthDir`: with a remote store, the auth directory is the
    /// store's mirror whatever `auth-dir` says.
    pub(crate) fn lock_auth_dir(&self, cfg: &mut Config) {
        if let Some(store) = &self.store {
            cfg.auth_dir = store.auth_dir();
        } else if let Some(dir) = &self.auth_dir {
            cfg.auth_dir = dir.clone();
        }
    }

    /// Go `deleteTokenRecord`: tells the store about an explicit removal. Blocking;
    /// call from a blocking thread.
    pub(crate) fn store_delete(&self, path: &std::path::Path) -> Result<(), String> {
        let Some(store) = self.store.clone() else {
            return Ok(());
        };
        let path = std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf());
        store.delete_auth(path).map_err(|e| format!("{e:#}"))
    }

    /// Go `Store.Save`: `write` through the configured store, which has the file when
    /// this returns (git syncs first and holds its lock across the write); without a
    /// store, just `write`. Blocking: call from a blocking thread holding `disk`.
    pub(crate) fn store_write(&self, write: crate::persist::SaveFn<'_>) -> Result<Option<PathBuf>, String> {
        match &self.store {
            Some(store) => store.save_auth(write),
            None => write(),
        }
        .map_err(|e| format!("{e:#}"))
    }

    /// Publishes a config and everything Go derives from it on reload: scheduler
    /// policy, management availability, and the credential set (auth-dir files plus
    /// config API keys). `files` replaces the auth-dir scan when the caller already
    /// has one; its file hashes keep a stale scan from undoing a later write (Go
    /// c4c1d43e). Callers hold `disk`. Infallible on purpose: access settings of a
    /// valid config (a rotated or removed secret) always take effect.
    pub(crate) fn publish(&self, cfg: Config, files: Option<credentials::AuthScan>) {
        self.publish_upload(cfg, files, None);
    }

    /// [`Self::publish`] after an upload wrote `upload`'s file. While plugin auth
    /// providers have not parsed the new bytes, the uploaded credential is registered as
    /// built, as Go's upload registers it (`buildAuthFromFileData`, no plugin parser),
    /// replacing one with the same ID; the watcher's next reload asks the plugins.
    /// `upload` carries the SHA-256 of the bytes written: a write committed since (a
    /// token refresh) changed the file, and the current credential is kept.
    pub(crate) fn publish_upload(
        &self,
        cfg: Config,
        files: Option<credentials::AuthScan>,
        upload: Option<(Credential, [u8; 32])>,
    ) {
        // Nothing below is applied for a config the runtime would refuse to publish.
        if !Runtime::accepts_config(&cfg) {
            return;
        }
        let mut cfg = cfg;
        self.lock_auth_dir(&mut cfg);
        use sha2::{Digest, Sha256};
        let scan = files.unwrap_or_else(|| credentials::scan_auth_dir(&cfg));
        let mut all = scan.credentials;
        let mut read = scan.read;
        // Uploads record absolute paths; a relative auth-dir scans relative ones.
        let absolute = |p: &PathBuf| std::path::absolute(p).unwrap_or_else(|_| p.clone());
        // Fallbacks follow their file: gone with it, replaced once a synthesizer
        // claims it, otherwise rebuilt from its current content.
        let mut claimed = Vec::new();
        let mut fallbacks = self.fallbacks.lock().unwrap_or_else(PoisonError::into_inner);
        fallbacks.retain(|path| {
            let Ok(data) = std::fs::read(path) else { return false };
            if all
                .iter()
                .any(|c| matches!(&c.source, Source::File(p) if absolute(p) == absolute(path)))
            {
                claimed.push(path.clone());
                return false;
            }
            match credentials::upload_fallback(&cfg.auth_dir, path, &data) {
                Some(c) => {
                    // Bound to the bytes it was built from, under its own path, so the
                    // freshness check compares this read, not the scan's.
                    if let Source::File(p) = &c.source {
                        read.insert(p.clone(), Sha256::digest(&data).into());
                    }
                    all.push(c);
                    true
                }
                None => false,
            }
        });
        drop(fallbacks);
        #[cfg(test)]
        {
            let mut hooks = BEFORE_RECONCILE.lock().unwrap();
            let hook = hooks
                .iter()
                .position(|(dir, _)| *dir == cfg.auth_dir)
                .map(|i| hooks.remove(i).1);
            drop(hooks);
            if let Some(hook) = hook {
                hook();
            }
        }
        all.extend(credentials::from_config(&cfg));
        self.access.config_published(&cfg);
        crate::logging::configure(&cfg);
        self.rt.usage_queue().configure(self.access.available(), &cfg);
        let policy = policy(&cfg);
        self.rt.publish_config_and_policy(cfg, policy);
        let upload = upload.filter(|(c, _)| {
            matches!(&c.source, Source::File(path) if scan.deferred.iter().any(|p| absolute(p) == absolute(path)))
        });
        if let Some((
            Credential {
                source: Source::File(path),
                ..
            },
            written,
        )) = &upload
        {
            read.insert(path.clone(), *written);
        }
        let rejected = self.rt.store().reconcile_scanned_with(all, &read, |current, all| {
            crate::plugins::keep_deferred(current, all, &scan.deferred);
            if let Some((upload, _)) = upload {
                all.retain(|c| c.id != upload.id);
                all.push(upload);
            }
        });
        // A synthesizer claimed the file but its credential was rejected as stale, so the
        // fallback stays live: keep tracking it, or the next reload would drop it.
        let kept: Vec<PathBuf> = claimed
            .into_iter()
            .filter(|path| rejected.iter().any(|r| absolute(r) == absolute(path)))
            .collect();
        if !kept.is_empty() {
            self.fallbacks
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .extend(kept);
        }
        // Go's watcher calls redisqueue.NotifyUsageRefresh after every client reload.
        self.rt.usage_queue().notify_usage_refresh();
    }
}

/// `Policy::from(routing)` plus the provider-level rules Go reads from config:
/// `oauth.request-scoped-errors`, sanitized like `SanitizeOAuthRequestScopedErrors`.
pub fn policy(cfg: &Config) -> Policy {
    let mut policy = Policy::from(&cfg.routing);
    let rules = cfg
        .document
        .get("oauth")
        .and_then(|o| o.get("request-scoped-errors"))
        .and_then(serde_yaml_ng::Value::as_mapping);
    for (channel, list) in rules.into_iter().flatten() {
        let channel = channel.as_str().unwrap_or_default().trim().go_lower();
        let clean: Vec<ErrorRule> = list
            .as_sequence()
            .into_iter()
            .flatten()
            .filter_map(|r| {
                let words = |k: &str| -> Vec<String> {
                    r.get(k)
                        .and_then(serde_yaml_ng::Value::as_sequence)
                        .into_iter()
                        .flatten()
                        .filter_map(|v| v.as_str().map(|s| s.trim().to_owned()))
                        .filter(|s| !s.is_empty())
                        .collect()
                };
                let rule = ErrorRule {
                    status: r.get("status").and_then(serde_yaml_ng::Value::as_i64).unwrap_or(0),
                    r#match: words("match"),
                    match_regex: words("match-regexr"),
                    action: r
                        .get("action")
                        .and_then(serde_yaml_ng::Value::as_str)
                        .unwrap_or_default()
                        .trim()
                        .go_lower(),
                };
                (rule.status > 0
                    && !(rule.r#match.is_empty() && rule.match_regex.is_empty())
                    && !rule.action.is_empty())
                .then_some(rule)
            })
            .collect();
        if !channel.is_empty() && !clean.is_empty() {
            policy.oauth_request_scoped_errors.insert(channel, clean);
        }
    }
    policy
}

/// gin `c.JSON` of a `gin.H` map: Go's `json.Marshal` (sorted keys; `<`, `>`, `&`,
/// U+2028 and U+2029 escaped) with the charset parameter.
pub(crate) fn json(status: StatusCode, value: &Value) -> Response {
    json_body(status, crate::gojson::sorted(value))
}

/// gin `c.JSON` of a value that holds Go structs: objects keep their insertion order
/// (struct fields as Go declares them; callers insert map levels sorted, see
/// [`sorted_keys`]), strings escaped like `json.Marshal`.
pub(crate) fn json_ordered(status: StatusCode, value: &Value) -> Response {
    let mut out = Vec::new();
    write_ordered(value, &mut out);
    json_body(status, String::from_utf8(out).expect("JSON text is UTF-8"))
}

fn write_ordered(value: &Value, out: &mut Vec<u8>) {
    match value {
        Value::Null => out.extend_from_slice(b"null"),
        Value::Bool(b) => out.extend_from_slice(if *b { b"true" } else { b"false" }),
        Value::Number(n) => out.extend_from_slice(n.to_string().as_bytes()),
        Value::String(s) => out.extend(cpa_common::json::quote(s)),
        Value::Array(items) => {
            out.push(b'[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(b',');
                }
                write_ordered(item, out);
            }
            out.push(b']');
        }
        Value::Object(map) => {
            out.push(b'{');
            for (i, (key, item)) in map.iter().enumerate() {
                if i > 0 {
                    out.push(b',');
                }
                out.extend(cpa_common::json::quote(key));
                out.push(b':');
                write_ordered(item, out);
            }
            out.push(b'}');
        }
    }
}

/// A copy whose objects all have sorted keys, as Go encodes a `map[string]any`.
pub(crate) fn sorted_keys(value: &Value) -> Value {
    match value {
        Value::Object(map) => {
            let sorted: std::collections::BTreeMap<&String, Value> =
                map.iter().map(|(k, v)| (k, sorted_keys(v))).collect();
            Value::Object(sorted.into_iter().map(|(k, v)| (k.clone(), v)).collect())
        }
        Value::Array(items) => Value::Array(items.iter().map(sorted_keys).collect()),
        other => other.clone(),
    }
}

fn json_body(status: StatusCode, body: String) -> Response {
    (
        status,
        [(
            header::CONTENT_TYPE,
            HeaderValue::from_static("application/json; charset=utf-8"),
        )],
        body,
    )
        .into_response()
}

pub(crate) fn json_error(status: StatusCode, message: &str) -> Response {
    json(status, &json!({"error": message}))
}

/// Go `HTTPStatusFromErrorOr(err, 400)` for a failed body read in the handlers
/// dd7b4334 changed: 413 at `server.max-request-body-bytes` (and at a route's own
/// `DefaultBodyLimit`), else 400.
pub(crate) fn read_status(rejection: &BytesRejection) -> StatusCode {
    if crate::body_limit::is_too_large(rejection) || rejection.status() == StatusCode::PAYLOAD_TOO_LARGE {
        StatusCode::PAYLOAD_TOO_LARGE
    } else {
        StatusCode::BAD_REQUEST
    }
}

/// The text of a failed body read: Go's `err.Error()` at the limit.
pub(crate) fn read_reason(rejection: &BytesRejection) -> String {
    crate::claude::read_error(rejection).1
}

fn error(code: u16, message: &str) -> Response {
    json_error(StatusCode::from_u16(code).unwrap(), message)
}

/// Wraps one method handler in the management guard; methods without a handler fall
/// through to a bare 404 like gin's NoRoute.
macro_rules! guarded {
    ($state:expr, $handler:expr) => {
        $handler.layer(middleware::from_fn_with_state($state.clone(), access::guard))
    };
}

/// Availability only, no key: Go's OAuth callback routes.
macro_rules! open {
    ($state:expr, $handler:expr) => {
        $handler.layer(middleware::from_fn_with_state($state.clone(), access::available))
    };
}

/// gin answers HEAD and unregistered methods on a known path with NoRoute (no `Allow`
/// header; starting from `any` keeps axum from adding one). Go's NoRoute serves
/// plugin routes, so those requests go to [`plugins::no_route`].
fn methods() -> MethodRouter<Arc<Management>> {
    // An explicit HEAD keeps axum from answering HEAD with the GET handler.
    any(plugins::no_route).head(plugins::no_route)
}

/// The OAuth callback POST needs no key, so unlike Go it keeps a bound: legitimate
/// bodies are a state, a code and a redirect URL.
const CALLBACK_BODY_LIMIT: usize = 64 * 1024;

pub fn router(state: Arc<Management>) -> Router {
    let s = &state;
    let v8 = "/v8/management";
    let v0 = "/v0/management";
    let config_methods = || {
        methods()
            .get(guarded!(s, config))
            .put(guarded!(s, config))
            .patch(guarded!(s, config))
            .delete(guarded!(s, config))
    };
    let mut router = Router::new()
        .route(
            &format!("{v8}/config"),
            methods()
                .get(guarded!(s, config))
                .put(guarded!(s, config))
                .patch(guarded!(s, config)),
        )
        .route(
            &format!("{v8}/config.yaml"),
            methods().get(guarded!(s, config)).put(guarded!(s, config)),
        )
        .route(&format!("{v8}/config/"), config_methods())
        .route(&format!("{v8}/config/{{*path}}"), config_methods())
        .route(
            &format!("{v0}/config.yaml"),
            methods()
                .get(guarded!(s, legacy_yaml))
                .put(guarded!(s, legacy::put_config_yaml)),
        );
    // v8 and its deprecated v0 spellings share Go's handlers.
    for (base, files, definitions) in [
        (v8, "credentials", "routing/model-definitions"),
        (v0, "auth-files", "model-definitions"),
    ] {
        router = router
            .route(
                &format!("{base}/{files}"),
                methods()
                    .get(guarded!(s, auth_files::list))
                    .post(guarded!(s, auth_files::upload))
                    .delete(guarded!(s, auth_files::delete)),
            )
            .route(
                &format!("{base}/{files}/models"),
                methods().get(guarded!(s, auth_files::models)),
            )
            .route(
                &format!("{base}/{files}/download"),
                methods().get(guarded!(s, auth_files::download)),
            )
            .route(
                &format!("{base}/{files}/status"),
                methods().patch(guarded!(s, auth_files::status)),
            )
            .route(
                &format!("{base}/{files}/fields"),
                methods().patch(guarded!(s, auth_files::fields)),
            )
            .route(
                &format!("{base}/{files}/refresh"),
                methods().post(guarded!(s, auth_files::refresh)),
            )
            .route(
                &format!("{base}/{definitions}/{{channel}}"),
                methods().get(guarded!(s, auth_files::model_definitions)),
            );
    }
    for (path, route) in [
        (
            "server/latest-version",
            methods().get(guarded!(s, observability::latest_version)),
        ),
        ("requests/api-call", methods().post(guarded!(s, api_call::api_call))),
        (
            "routing/cooldown/reset",
            methods().post(guarded!(s, auth_files::cooldown_reset)),
        ),
        (
            "observability/usage/api-keys",
            methods().get(guarded!(s, observability::api_key_usage)),
        ),
        (
            "observability/usage/queue",
            methods().get(guarded!(s, observability::usage_queue)),
        ),
        ("oauth/auth-url", methods().get(guarded!(s, oauth::auth_url))),
        ("oauth/status", methods().get(guarded!(s, oauth::status))),
        ("oauth/session", methods().delete(guarded!(s, oauth::cancel))),
        ("oauth/import", methods().post(guarded!(s, oauth::import))),
        ("oauth/siwc/bind", methods().post(guarded!(s, siwc::bind))),
        (
            "observability/logs",
            methods()
                .get(guarded!(s, logs::get_logs))
                .delete(guarded!(s, logs::delete_logs)),
        ),
        (
            "observability/logs/errors",
            methods().get(guarded!(s, logs::error_logs)),
        ),
        (
            "observability/logs/errors/{name}",
            methods().get(guarded!(s, logs::download_error_log)),
        ),
        (
            "observability/logs/requests/{id}",
            methods().get(guarded!(s, logs::request_log)),
        ),
        (
            "oauth/callback",
            methods()
                .get(open!(s, oauth::callback_get))
                .post(open!(s, oauth::callback_post).layer(DefaultBodyLimit::max(CALLBACK_BODY_LIMIT))),
        ),
    ] {
        router = router.route(&format!("{v8}/{path}"), route);
    }
    for (path, route) in [
        (
            "latest-version",
            methods().get(guarded!(s, observability::latest_version)),
        ),
        ("api-call", methods().post(guarded!(s, api_call::api_call))),
        ("reset-quota", methods().post(guarded!(s, auth_files::cooldown_reset))),
        (
            "api-key-usage",
            methods().get(guarded!(s, observability::api_key_usage)),
        ),
        ("usage-queue", methods().get(guarded!(s, observability::usage_queue))),
        ("get-auth-status", methods().get(guarded!(s, oauth::status))),
        ("oauth-session", methods().delete(guarded!(s, oauth::cancel))),
        (
            "oauth-callback",
            methods()
                .get(open!(s, oauth::callback_get))
                .post(open!(s, oauth::callback_post).layer(DefaultBodyLimit::max(CALLBACK_BODY_LIMIT))),
        ),
    ] {
        router = router.route(&format!("{v0}/{path}"), route);
    }
    // Plugin routes: v8 and the v0 spellings Go keeps (internal/api/server_management.go,
    // server_management_v8.go); the v0 config, enable and v8 store routes share them.
    for (path, route) in [
        ("plugins", methods().get(guarded!(s, plugins::list))),
        ("plugins/{id}", methods().delete(guarded!(s, plugins::delete))),
    ] {
        router = router
            .route(&format!("{v8}/{path}"), route.clone())
            .route(&format!("{v0}/{path}"), route);
    }
    // Plugin store (plugin_store.go). gin keeps one route tree per method, so a DELETE of
    // `plugins/store` is DeletePlugin with the ID `store`.
    router = router
        .route(
            &format!("{v8}/plugins/store"),
            methods()
                .get(guarded!(s, plugin_store::list))
                .delete(guarded!(s, plugins::delete_store_id)),
        )
        .route(
            &format!("{v8}/plugins/store/{{id}}/install"),
            methods().post(guarded!(s, plugin_store::install)),
        )
        .route(
            &format!("{v0}/plugin-store"),
            methods().get(guarded!(s, plugin_store::list)),
        )
        .route(
            &format!("{v0}/plugin-store/{{id}}/install"),
            methods().post(guarded!(s, plugin_store::install)),
        );
    // Plugin quota (plugin_quota.go): per-plugin under v8 and v0, the credential-level
    // routes and the reset alias only under v0.
    for base in [v8, v0] {
        router = router.route(
            &format!("{base}/plugins/{{id}}/quota"),
            methods()
                .get(guarded!(s, quota::get_plugin))
                .post(guarded!(s, quota::fetch_plugin))
                .delete(guarded!(s, quota::reset_plugin)),
        );
    }
    router = router
        .route(
            &format!("{v0}/plugins/{{id}}/quota/reset"),
            methods().post(guarded!(s, quota::reset_plugin)),
        )
        .route(
            &format!("{v0}/quota/providers"),
            methods().get(guarded!(s, quota::providers)),
        )
        .route(&format!("{v0}/quota/fetch"), methods().post(guarded!(s, quota::fetch)))
        .route(&format!("{v0}/quota/reset"), methods().post(guarded!(s, quota::reset)));
    router = router
        .route(
            &format!("{v0}/plugins/{{id}}/enabled"),
            methods().patch(guarded!(s, plugins::patch_enabled)),
        )
        .route(
            &format!("{v0}/plugins/{{id}}/config"),
            methods()
                .get(guarded!(s, plugins::get_config))
                .put(guarded!(s, plugins::put_config))
                .patch(guarded!(s, plugins::patch_config)),
        );
    // Go's remaining v0 routes, adapted over the v8 handlers (management/legacy.rs).
    router = router.route(&format!("{v0}/config"), methods().get(guarded!(s, legacy::config)));
    for path in legacy::FIELD_ROUTES {
        let mut route = methods()
            .get(guarded!(s, legacy::field_route))
            .put(guarded!(s, legacy::field_route))
            .patch(guarded!(s, legacy::field_route));
        if path == "proxy-url" {
            route = route.delete(guarded!(s, legacy::field_route));
        }
        router = router.route(&format!("{v0}/{path}"), route);
    }
    router = router.route(
        &format!("{v0}/api-keys"),
        methods()
            .get(guarded!(s, legacy::api_keys))
            .put(guarded!(s, legacy::api_keys))
            .patch(guarded!(s, legacy::api_keys))
            .delete(guarded!(s, legacy::api_keys)),
    );
    for path in legacy::LIST_ROUTES {
        router = router.route(
            &format!("{v0}/{path}"),
            methods()
                .get(guarded!(s, legacy::list_route))
                .put(guarded!(s, legacy::list_route))
                .patch(guarded!(s, legacy::list_route))
                .delete(guarded!(s, legacy::list_route)),
        );
    }
    for (path, _) in legacy::AUTH_URL_ROUTES {
        router = router.route(&format!("{v0}/{path}"), methods().get(guarded!(s, legacy::auth_url)));
    }
    router = router
        .route(
            &format!("{v0}/vertex/import"),
            methods().post(guarded!(s, legacy::vertex_import)),
        )
        .route(
            &format!("{v0}/logs"),
            methods()
                .get(guarded!(s, logs::get_logs))
                .delete(guarded!(s, logs::delete_logs)),
        )
        .route(
            &format!("{v0}/request-error-logs"),
            methods().get(guarded!(s, logs::error_logs)),
        )
        .route(
            &format!("{v0}/request-error-logs/{{name}}"),
            methods().get(guarded!(s, logs::download_error_log)),
        )
        .route(
            &format!("{v0}/request-log-by-id/{{id}}"),
            methods().get(guarded!(s, logs::request_log)),
        );
    router
        .fallback(plugins::no_route)
        .route("/management.html", get(panel))
        .route("/assets/{*path}", get(panel))
        .route("/fonts/{*path}", get(panel))
        .route("/favicon.svg", get(panel))
        // gin reads management bodies without a size limit (uploads, config writes);
        // they are only read after authentication.
        .layer(DefaultBodyLimit::disable())
        .layer(middleware::from_fn(cors))
        .with_state(state)
}

pub(crate) async fn config(
    State(state): State<Arc<Management>>,
    OriginalUri(uri): OriginalUri,
    method: Method,
    headers: HeaderMap,
    body: axum::body::Body,
) -> Response {
    // gin's *path parameter is the decoded path.
    let path = percent_decode(uri.path());
    let if_match: Vec<_> = headers
        .get_all(header::IF_MATCH)
        .iter()
        .map(|v| String::from_utf8_lossy(v.as_bytes()).into_owned())
        .collect();
    // Go reads the body only for writes other than DELETE, under its lock, after the file
    // read, the If-Match check and the stored config's parse. Those run here under the
    // disk lock, which is released while the body arrives, and again under it before
    // the write.
    let body = if matches!(method, Method::GET | Method::DELETE) {
        Ok(Bytes::new())
    } else {
        let check = {
            let (state, method, if_match) = (state.clone(), method.clone(), if_match.clone());
            tokio::task::spawn_blocking(move || {
                let _guard = state.disk.lock().unwrap_or_else(PoisonError::into_inner);
                stored(&state, &method, &if_match).err()
            })
        };
        match check.await {
            Ok(Some(response)) => return *response,
            Ok(None) => {}
            Err(_) => return error(500, "internal_error"),
        }
        axum::body::to_bytes(body, usize::MAX).await.map_err(|e| {
            if crate::body_limit::is_too_large(&e) {
                StatusCode::PAYLOAD_TOO_LARGE
            } else {
                StatusCode::BAD_REQUEST
            }
        })
    };
    tokio::task::spawn_blocking(move || config_sync(&state, &path, method, &if_match, body.as_deref().map_err(|s| *s)))
        .await
        .unwrap_or_else(|_| error(500, "internal_error"))
}

pub(crate) fn percent_decode(path: &str) -> String {
    let bytes = path.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let hex = bytes
            .get(i + 1..i + 3)
            .and_then(|h| u8::from_str_radix(std::str::from_utf8(h).ok()?, 16).ok());
        match (bytes[i], hex) {
            (b'%', Some(b)) => {
                out.push(b);
                i += 3;
            }
            (b, _) => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Root-level comment lines after the last mapping entry.
fn foot_comments(text: &str) -> String {
    let lines: Vec<&str> = text.lines().collect();
    let last = lines
        .iter()
        .rposition(|l| !l.trim().is_empty() && !l.starts_with('#'))
        .map_or(0, |i| i + 1);
    lines[last..]
        .iter()
        .filter(|l| l.starts_with('#'))
        .map(|l| format!("{l}\n"))
        .collect()
}

fn invalid_config(status: StatusCode, err: impl std::fmt::Display) -> Response {
    json(status, &json!({"error": "invalid_config", "message": err.to_string()}))
}

/// Stores the bcrypt hash of a plaintext management key at `path`. `decoded` is the
/// key as the config decodes it, so a number or bool written there (`123`) is hashed as
/// its string, as Go's `ParseConfigBytes` does before publishing.
fn hash_secret(doc: &mut ConfigDocument, path: &[&str], decoded: &str) -> Option<Response> {
    if decoded.is_empty() || is_bcrypt(decoded) {
        return None;
    }
    match bcrypt::hash(decoded, bcrypt::DEFAULT_COST) {
        Err(e) => Some(invalid_config(StatusCode::UNPROCESSABLE_ENTITY, e)),
        Ok(hash) => doc
            .update(path, hash.into(), false)
            .err()
            .map(|_| error(422, "invalid_config")),
    }
}

/// Go `Handler.ConfigV8`: every call reads the file and migrates it in memory (legacy
/// layout to v8, unknown sections archived as comments); only successful mutations
/// persist. The write keeps untouched text byte-stable instead of Go's re-encoding.
///
/// Reads carry an `ETag` with the SHA-256 of the file bytes; a write with `If-Match`
/// lines that do not name the current version fails with 412 (v8.0.35 `fa68946a`).
pub(crate) fn config_sync(
    state: &Management,
    path: &str,
    method: Method,
    if_match: &[String],
    body: Result<&[u8], StatusCode>,
) -> Response {
    let _guard = state.disk.lock().unwrap_or_else(PoisonError::into_inner);
    config_sync_locked(state, path, method, if_match, body, false)
}

/// The stored config as Go's `ConfigV8` prepares it before reading a body: the file
/// bytes and their `ETag`, the `If-Match` check for writes, then the parsed document.
/// Errors are Go's answers. Call it holding `state.disk`.
fn stored(
    state: &Management,
    method: &Method,
    if_match: &[String],
) -> Result<(String, HeaderValue, ConfigDocument), Box<Response>> {
    let raw = std::fs::read(&state.path).map_err(|_| Box::new(error(500, "read_failed")))?;
    let etag = config_etag(&raw);
    if method != Method::GET && !if_match_satisfied(if_match, &etag) {
        let mut response = error(412, "precondition_failed");
        response.headers_mut().insert(header::ETAG, etag);
        return Err(Box::new(response));
    }
    let invalid = |e: &dyn std::fmt::Display| Box::new(invalid_config(StatusCode::INTERNAL_SERVER_ERROR, e));
    let original = String::from_utf8(raw).map_err(|_| invalid(&"yaml: invalid leading UTF-8 octet"))?;
    let doc = ConfigDocument::parse(&original).map_err(|e| invalid(&e))?;
    Ok((original, etag, doc))
}

/// [`config_sync`] for a caller already holding `state.disk`. `typed` saves the written
/// values as Go's typed v0 saver does (`SaveConfigPreserveComments`); a v8 write saves
/// the validated tree as written (Go v8.0.35).
fn config_sync_locked(
    state: &Management,
    path: &str,
    method: Method,
    if_match: &[String],
    body: Result<&[u8], StatusCode>,
    typed: bool,
) -> Response {
    let (original, etag, mut doc) = match stored(state, &method, if_match) {
        Ok(v) => v,
        Err(response) => return *response,
    };
    let basis = doc.migrated_text(&original).unwrap_or_else(|| original.clone());
    let archived = doc.archive_unknown();
    // Go keys this on the matched route: `/config/config.yaml` is a key lookup.
    let yaml = matches!(path, "/v8/management/config.yaml" | "/v0/management/config.yaml");
    let suffix = path
        .strip_prefix("/v8/management/config/")
        .unwrap_or_default()
        .trim_matches('/');
    let parts: Vec<_> = if suffix.is_empty() {
        vec![]
    } else {
        suffix.split('/').collect()
    };
    if method == Method::GET {
        // Go v8.0.35 (3be5fa44): a historical path reads the canonical values, with
        // the file's YAML types. Cost: a GET parses the file twice, for the write
        // document (`stored`) and for this view.
        let mut view = match ConfigDocument::view(&original) {
            Ok(view) => view,
            Err(e) => return invalid_config(StatusCode::INTERNAL_SERVER_ERROR, e),
        };
        view.archive_unknown();
        view.project_aliases(&parts);
        if yaml {
            return match doc.render_preserving(&basis) {
                Ok(text) => (
                    [
                        (
                            header::CONTENT_TYPE,
                            HeaderValue::from_static("application/yaml; charset=utf-8"),
                        ),
                        (header::CACHE_CONTROL, HeaderValue::from_static("no-store")),
                        (header::ETAG, etag),
                    ],
                    text + &archive_comments(&archived),
                )
                    .into_response(),
                Err(_) => error(500, "decode_failed"),
            };
        }
        let mut result = match serde_json::to_value(cpa_core::config::AsJson(view.value())) {
            Ok(v) => v,
            Err(_) => return error(500, "decode_failed"),
        };
        // Go parses the stored document for this, falling back to the live config.
        let cfg = doc.yaml().ok().and_then(|text| Config::parse(&text).ok());
        let cfg = cfg.map_or_else(|| state.rt.config(), Arc::new);
        auth_index::inject(&mut result, &cfg, &live_auth_indexes(state));
        // TURN passwords are omitted from JSON reads, never from YAML downloads.
        if let Some(servers) = result
            .pointer_mut("/oauth/providers/codex/live-media-relay/ice-servers")
            .and_then(Value::as_array_mut)
        {
            for server in servers {
                if let Some(map) = server.as_object_mut() {
                    map.remove("username");
                    map.remove("credential");
                }
            }
        }
        let mut selected = &result;
        for part in &parts {
            let Some(next) = selected.as_object().and_then(|o| o.get(*part)) else {
                return error(404, "not_found");
            };
            selected = next;
        }
        let mut response = json(StatusCode::OK, selected);
        let headers = response.headers_mut();
        headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
        headers.insert(header::ETAG, etag);
        return response;
    }
    let before = doc.clone();
    // A write to a historical path replaces or deletes the canonical values there.
    doc.project_aliases(&parts);
    let mut written = None;
    if method == Method::DELETE {
        if parts.is_empty() {
            return error(400, "cannot_delete_config");
        }
        if !doc.delete(&parts) {
            return error(404, "not_found");
        }
    } else {
        let body = match body {
            Ok(body) => body,
            Err(status) => return error(status.as_u16(), "invalid_body"),
        };
        if !yaml && serde_json::from_slice::<serde::de::IgnoredAny>(body).is_err() {
            return error(400, "invalid_json");
        }
        // yaml.v3 yields no document for an empty or comment-only body (an explicit
        // `null` is a document).
        let has_document = String::from_utf8_lossy(body).lines().any(|l| {
            let t = l.trim();
            !t.is_empty() && !t.starts_with('#') && t != "---" && t != "..."
        });
        let parsed = std::str::from_utf8(body)
            .map_err(|_| ())
            .and_then(|t| cpa_core::config::parse_yaml(t).map_err(drop));
        let mut value = match parsed {
            Ok(v) if has_document => v,
            _ => return error(400, "invalid_body"),
        };
        if parts.is_empty() && !value.is_mapping() {
            return error(400, "config_must_be_object");
        }
        // Go keeps a `!!binary` node tagged through the update, after the root shape
        // check; decoding it for the save fails on invalid base64 with an invalid
        // config (400). A string setting written as a plain number keeps its text, as
        // the write document holds it ([`ConfigDocument::parse`]).
        if yaml {
            value = match std::str::from_utf8(body)
                .map_err(anyhow::Error::from)
                .and_then(cpa_core::config::config_value)
            {
                Ok(v) => v,
                Err(e) => return invalid_config(StatusCode::BAD_REQUEST, e),
            };
        } else if let Ok(v) = std::str::from_utf8(body)
            .map_err(anyhow::Error::from)
            .and_then(|text| cpa_core::config::body_value(text, &parts))
        {
            // A JSON number in a string setting saves as Go saves its node: its text.
            value = v;
        }
        // A YAML upload is alias-normalized first, where a scalar merge fails (Go
        // `NormalizeV8ConfigAliases`, 400). JSON keys are quoted, so Go keeps `<<` as a
        // plain key through the update and expands it only when it parses its own save,
        // which writes it plain: below.
        if yaml && let Err(e) = cpa_core::config::expand_merges(&mut value) {
            return invalid_config(StatusCode::BAD_REQUEST, e);
        }
        auth_index::strip_update(&parts, &mut value);
        if parts.is_empty() {
            ConfigDocument::normalize_aliases(&mut value);
        }
        if doc.update(&parts, value.clone(), method == Method::PATCH).is_err() {
            return error(400, "invalid_path");
        }
        if !yaml {
            doc.preserve_turn_secrets(&before);
        }
        written = Some(value);
        // The completed candidate, as Go reparses it: keys already there win over merged
        // ones, and a scalar merge is an invalid config (422).
        if !yaml && let Err(e) = cpa_core::config::expand_merges(doc.value_mut()) {
            return invalid_config(StatusCode::UNPROCESSABLE_ENTITY, e);
        }
    }
    ConfigDocument::normalize_aliases(doc.value_mut());
    // Go saves every v8 write as `NormalizeConfigLayout` output, which declares the
    // version even when a whole-config PUT omitted it.
    if let Some(root) = doc.value_mut().as_mapping_mut()
        && !root.contains_key("config-version")
    {
        root.insert("config-version".into(), 8.into());
    }
    auth_index::strip_root(doc.value_mut());
    for field in [
        "credentials/concurrency/lifecycle-config-revision",
        "credentials/concurrency/observation-barrier-revision",
        "plugins/auth-revision",
    ] {
        let parts: Vec<_> = field.split('/').collect();
        if doc.get(&parts) != before.get(&parts) {
            return json(
                StatusCode::BAD_REQUEST,
                &json!({"error": "read_only_field", "field": field}),
            );
        }
    }
    let text = match doc.yaml() {
        Ok(v) => v,
        Err(e) => return invalid_config(StatusCode::BAD_REQUEST, e),
    };
    // Go `ParseConfigBytes` (422), which checks the layout shape, the key groups and
    // the weights unwrapped before it decodes, then `ValidateV8Config` (400) the layout
    // before its strict decode.
    let parsed = match Config::parse_payload(&text) {
        Ok(v) => v,
        Err(e) => return invalid_config(StatusCode::UNPROCESSABLE_ENTITY, e),
    };
    if let Some(e) = cpa_core::config::v8_layout_error(doc.value()) {
        return invalid_config(StatusCode::BAD_REQUEST, e);
    }
    if let Err(e) = cpa_core::config::validate_config_fields(doc.value(), true) {
        return invalid_config(StatusCode::BAD_REQUEST, e);
    }
    // Go's v0 saver persists typed values; projecting only after validation keeps
    // malformed input from being sanitized into success.
    if typed && let Some(written) = &written {
        doc.typed_projection(&parts, written, method == Method::PATCH);
    }
    if let Some(response) = hash_secret(&mut doc, &["management", "secret-key"], &parsed.management.secret_key) {
        return response;
    }
    let basis = if yaml {
        match std::str::from_utf8(body.unwrap_or_default()) {
            Ok(v) => v.to_owned(),
            Err(_) => return error(400, "invalid_body"),
        }
    } else {
        basis
    };
    let mut text = match doc.render_preserving(&basis) {
        Ok(v) => v,
        Err(e) => return invalid_config(StatusCode::INTERNAL_SERVER_ERROR, e),
    };
    // Go keeps document-level foot comments (its archive of unknown sections) even
    // when the root mapping is replaced by a YAML upload.
    if yaml {
        text += &foot_comments(&original);
    }
    text += &archive_comments(&archived);
    let cfg = match Config::parse_payload(&text) {
        Ok(v) => v,
        Err(e) => return invalid_config(StatusCode::UNPROCESSABLE_ENTITY, e),
    };
    if let Err(e) = ConfigDocument::write(&state.path, &text) {
        return json(
            StatusCode::INTERNAL_SERVER_ERROR,
            &json!({"error": "write_failed", "message": e.to_string()}),
        );
    }
    state.publish(cfg, None);
    let mut response = json(StatusCode::OK, &json!({"config-version": 8, "status": "ok"}));
    // The version actually on disk; none when it cannot be read back.
    if let Ok(saved) = std::fs::read(&state.path) {
        response.headers_mut().insert(header::ETAG, config_etag(&saved));
    }
    response
}

/// What a v0 save does at its legacy path.
pub(crate) enum LegacyEdit {
    Set(serde_json::Value),
    Delete,
    /// Rewrite unchanged, as Go saves after a handler that changed nothing.
    Keep,
}

/// A v0 save at the v8 path: `Some(value)` replaces it, `None` deletes it.
pub(crate) struct V8Edit<'a> {
    pub path: &'a str,
    pub value: Option<Value>,
}

/// Test hook run once by the next [`Management::publish`] for the auth folder it names,
/// after it has built its fallback credentials and before it reconciles them.
#[cfg(test)]
pub(crate) static BEFORE_RECONCILE: Mutex<Vec<(PathBuf, LayoutHook)>> = Mutex::new(Vec::new());

/// Test hook run once, for the config file it names, when [`v0_save`] has chosen the v8
/// writer and not yet written: the boundary where an unlocked save would let another
/// writer in.
#[cfg(test)]
pub(crate) static AFTER_LAYOUT: Mutex<Option<(PathBuf, LayoutHook)>> = Mutex::new(None);
#[cfg(test)]
pub(crate) type LayoutHook = Box<dyn FnOnce() + Send>;

/// Go v8.0.35 `SaveConfigPreserveComments`: a v0 save keeps a file without v8 sections
/// in its legacy layout, so legacy settings keep their scope (a legacy
/// `codex-header-defaults` still applies to API keys); a v8 file saves through the v8
/// writer. The caller holds the `disk` lock across the layout check and the chosen
/// write, so an upload cannot change the layout in between, and across reading the
/// value it changes, so a concurrent save cannot be lost (Go 1631875b).
/// ponytail: the legacy branch changes only the edited key; Go also merges its whole
/// typed legacy render into the file and drops removed integration keys. Port
/// `mergeMappingPreserve` here if a difference in the saved file matters.
pub(crate) fn v0_save(
    state: &Management,
    _disk: &std::sync::MutexGuard<'_, ()>,
    legacy: &[&str],
    edit: LegacyEdit,
    v8: V8Edit,
) -> Response {
    let original = match std::fs::read_to_string(&state.path) {
        Ok(v) => v,
        Err(_) => return error(500, "read_failed"),
    };
    // Go's saver works on the alias- and merge-expanded root mapping (`flattenV8`).
    let mut root = match cpa_core::config::config_value(&original) {
        Ok(v) => v,
        Err(e) => return invalid_config(StatusCode::INTERNAL_SERVER_ERROR, e),
    };
    if let Err(e) = cpa_core::config::expand_merges(&mut root) {
        return invalid_config(StatusCode::INTERNAL_SERVER_ERROR, e);
    }
    if !root.is_mapping() {
        return invalid_config(StatusCode::INTERNAL_SERVER_ERROR, "expected root mapping node");
    }
    if cpa_core::config::is_v8_layout(&root) {
        #[cfg(test)]
        {
            let hook = {
                let mut slot = AFTER_LAYOUT.lock().unwrap_or_else(PoisonError::into_inner);
                match slot.as_ref() {
                    Some((path, _)) if *path == state.path => slot.take(),
                    _ => None,
                }
            };
            if let Some((_, hook)) = hook {
                hook();
            }
        }
        return match v8.value {
            Some(value) => config_sync_locked(state, v8.path, Method::PUT, &[], Ok(value.to_string().as_bytes()), true),
            None => config_sync_locked(state, v8.path, Method::DELETE, &[], Ok(b""), true),
        };
    }
    let mut doc = ConfigDocument::unmigrated(root);
    match edit {
        LegacyEdit::Set(value) => {
            let value = match serde_yaml_ng::to_value(value) {
                Ok(v) => v,
                Err(e) => return invalid_config(StatusCode::BAD_REQUEST, e),
            };
            if doc.update(legacy, value, false).is_err() {
                return error(400, "invalid_path");
            }
        }
        LegacyEdit::Delete => {
            doc.delete(legacy);
        }
        LegacyEdit::Keep => {}
    }
    // Go saves its in-memory config, whose key is hashed on load; a plaintext key
    // edited into the file since must not be published.
    let parsed = match doc.yaml().and_then(|t| Config::parse(&t)) {
        Ok(v) => v,
        Err(e) => return invalid_config(StatusCode::UNPROCESSABLE_ENTITY, e),
    };
    if let Some(response) = hash_secret(
        &mut doc,
        &["remote-management", "secret-key"],
        &parsed.management.secret_key,
    ) {
        return response;
    }
    let text = match doc.render_preserving(&original) {
        Ok(v) => v,
        Err(e) => return invalid_config(StatusCode::INTERNAL_SERVER_ERROR, e),
    };
    let cfg = match Config::parse(&text) {
        Ok(v) => v,
        Err(e) => return invalid_config(StatusCode::UNPROCESSABLE_ENTITY, e),
    };
    if let Err(e) = ConfigDocument::write(&state.path, &text) {
        return json(
            StatusCode::INTERNAL_SERVER_ERROR,
            &json!({"error": "write_failed", "message": e.to_string()}),
        );
    }
    state.publish(cfg, None);
    json(StatusCode::OK, &json!({"status": "ok"}))
}

/// Go `liveAuthIndexFromManager`: the index each config-backed credential carries.
fn live_auth_indexes(state: &Management) -> std::collections::HashMap<String, String> {
    state
        .rt
        .store()
        .snapshot()
        .iter()
        .filter(|c| matches!(c.source, Source::Config { .. }))
        .map(|c| (c.id.clone(), credentials::auth_index(c)))
        .collect()
}

/// Go `configV8ETag(ComputeSourceHash(raw))`: a strong tag of the file's SHA-256.
fn config_etag(raw: &[u8]) -> HeaderValue {
    use sha2::{Digest, Sha256};
    let hex: String = Sha256::digest(raw).iter().map(|b| format!("{b:02x}")).collect();
    HeaderValue::try_from(format!("\"{hex}\"")).expect("hex is a valid header value")
}

/// Go `ifMatchSatisfied`: RFC 9110 strong comparison over every `If-Match` line as one
/// list. Absent passes, blank fails, a whole-field `*` matches any version, and weak
/// or malformed tags never match. A comma inside a quoted tag is not a separator.
fn if_match_satisfied(values: &[String], etag: &HeaderValue) -> bool {
    if values.is_empty() {
        return true;
    }
    // `strings.TrimSpace`: Unicode white space too.
    let trim = |s: &[u8]| cpa_common::gostr::trim_space(s).to_vec();
    let field = trim(values.join(",").as_bytes());
    if field == b"*" {
        return true;
    }
    let mut quoted = false;
    field
        .split(|&c| {
            quoted ^= c == b'"';
            c == b',' && !quoted
        })
        .any(|tag| trim(tag) == etag.as_bytes())
}

async fn legacy_yaml(State(state): State<Arc<Management>>) -> Response {
    match tokio::fs::read(&state.path).await {
        Ok(bytes) => ([(header::CONTENT_TYPE, "application/yaml; charset=utf-8")], bytes).into_response(),
        Err(_) => error(404, "not_found"),
    }
}

include!(concat!(env!("OUT_DIR"), "/dashboard.rs"));

async fn panel(State(state): State<Arc<Management>>, OriginalUri(uri): OriginalUri) -> Response {
    if state.rt.config().management.disable_control_panel {
        return error(404, "not_found");
    }
    let Some((bytes, mime)) = dashboard_asset(uri.path()) else {
        return error(404, "not_found");
    };
    (
        [(header::CONTENT_TYPE, mime), (header::CACHE_CONTROL, "no-cache")],
        bytes,
    )
        .into_response()
}
