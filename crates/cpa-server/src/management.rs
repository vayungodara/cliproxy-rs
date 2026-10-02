//! Management API (v8, plus the few legacy v0 reads the dashboard predates).
//!
//! Routing follows gin: an unknown path or an unregistered method on a known path is a
//! bare 404 that never reaches authentication. Access rules live in [`access`].
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, PoisonError};

use axum::body::Bytes;
use axum::extract::{OriginalUri, State};
use axum::handler::Handler;
use axum::http::{HeaderValue, Method, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{MethodRouter, any, get};
use axum::{Router, middleware};
use cpa_core::config::{Config, ConfigDocument, credentials, is_bcrypt};
use cpa_core::credential::{Credential, MetadataPatch, Source};
use serde_json::{Value, json};

use crate::Runtime;
use crate::scheduler::{ErrorRule, Policy};

mod access;
pub use access::cors;

pub struct Management {
    pub(crate) rt: Arc<Runtime>,
    pub(crate) path: PathBuf,
    // ponytail: one lock for config and auth disk operations; split only if management
    // throughput matters. The watcher shares it, preventing stale disk publication.
    pub(crate) disk: Mutex<()>,
    access: access::Access,
}

#[derive(Default)]
pub struct Options {
    /// `--password`: accepted from loopback clients only, like Go's local password.
    pub local_password: String,
    /// Overrides the `MANAGEMENT_PASSWORD` environment variable when set.
    pub management_password: Option<String>,
}

impl Management {
    pub fn new(rt: Arc<Runtime>, path: PathBuf) -> Arc<Self> {
        Self::with_options(rt, path, Options::default())
    }

    /// Captures startup-only settings (trusted proxies, environment secret) and
    /// publishes the scheduler policy derived from the current config.
    pub fn with_options(rt: Arc<Runtime>, path: PathBuf, options: Options) -> Arc<Self> {
        let cfg = rt.config();
        rt.publish_policy(policy(&cfg));
        Arc::new(Self {
            access: access::Access::new(&cfg, options),
            rt,
            path,
            disk: Mutex::new(()),
        })
    }

    /// Publishes a config and everything Go derives from it on reload: scheduler
    /// policy, management availability, and the credential set (auth-dir files plus
    /// config API keys). `files` replaces the auth-dir scan when the caller already
    /// has a reconciled list. Callers hold `disk`. Infallible on purpose: access
    /// settings of a valid config (a rotated or removed secret) always take effect.
    pub(crate) fn publish(&self, cfg: Config, files: Option<Vec<Credential>>) {
        let mut all = files.unwrap_or_else(|| credentials::from_auth_dir(&cfg));
        all.extend(credentials::from_config(&cfg));
        self.access.config_published(&cfg);
        let policy = policy(&cfg);
        self.rt.publish_config_and_policy(cfg, policy);
        self.rt.store().reconcile(all);
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
        let channel = channel.as_str().unwrap_or_default().trim().to_lowercase();
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
                        .to_lowercase(),
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

/// gin `c.JSON`: compact JSON with the charset parameter.
pub(crate) fn json(status: StatusCode, value: &Value) -> Response {
    (
        status,
        [(
            header::CONTENT_TYPE,
            HeaderValue::from_static("application/json; charset=utf-8"),
        )],
        value.to_string(),
    )
        .into_response()
}

pub(crate) fn json_error(status: StatusCode, message: &str) -> Response {
    json(status, &json!({"error": message}))
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

/// gin answers HEAD and unregistered methods on a known path with NoRoute's 404 and
/// no `Allow` header; starting from `any` keeps axum from adding one.
fn methods() -> MethodRouter<Arc<Management>> {
    any(|| async { access::not_found() }).head(|| async { access::not_found() })
}

pub fn router(state: Arc<Management>) -> Router {
    let s = &state;
    let v8 = "/v8/management";
    let v0 = "/v0/management";
    Router::new()
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
        .route(
            &format!("{v8}/config/"),
            methods()
                .get(guarded!(s, config))
                .put(guarded!(s, config))
                .patch(guarded!(s, config))
                .delete(guarded!(s, config)),
        )
        .route(
            &format!("{v8}/config/{{*path}}"),
            methods()
                .get(guarded!(s, config))
                .put(guarded!(s, config))
                .patch(guarded!(s, config))
                .delete(guarded!(s, config)),
        )
        .route(&format!("{v8}/credentials"), methods().get(guarded!(s, credentials)))
        .route(
            &format!("{v8}/credentials/download"),
            methods().get(guarded!(s, download)),
        )
        .route(
            &format!("{v8}/credentials/status"),
            methods().patch(guarded!(s, status)),
        )
        .route(&format!("{v0}/config.yaml"), methods().get(guarded!(s, legacy_yaml)))
        .route(&format!("{v0}/auth-files"), methods().get(guarded!(s, credentials)))
        .route(
            &format!("{v0}/auth-files/download"),
            methods().get(guarded!(s, download)),
        )
        .route(&format!("{v0}/auth-files/status"), methods().patch(guarded!(s, status)))
        .route("/management.html", get(panel))
        .route("/assets/{*path}", get(panel))
        .route("/fonts/{*path}", get(panel))
        .route("/favicon.svg", get(panel))
        .layer(middleware::from_fn(cors))
        .with_state(state)
}

async fn config(
    State(state): State<Arc<Management>>,
    OriginalUri(uri): OriginalUri,
    method: Method,
    body: Bytes,
) -> Response {
    // gin's *path parameter is the decoded path.
    let path = percent_decode(uri.path());
    tokio::task::spawn_blocking(move || config_sync(&state, &path, method, &body))
        .await
        .unwrap_or_else(|_| error(500, "internal_error"))
}

fn percent_decode(path: &str) -> String {
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

fn config_sync(state: &Management, path: &str, method: Method, body: &[u8]) -> Response {
    let _guard = state.disk.lock().unwrap_or_else(PoisonError::into_inner);
    let original = match std::fs::read_to_string(&state.path) {
        Ok(v) => v,
        Err(_) => return error(500, "read_failed"),
    };
    let mut doc = match ConfigDocument::parse(&original) {
        Ok(v) => v,
        Err(_) => return error(500, "invalid_config"),
    };
    let yaml = path.ends_with("/config.yaml");
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
        if yaml {
            return match doc.render_preserving(&original) {
                Ok(text) => (
                    [
                        (header::CONTENT_TYPE, "application/yaml; charset=utf-8"),
                        (header::CACHE_CONTROL, "no-store"),
                    ],
                    text,
                )
                    .into_response(),
                Err(_) => error(500, "encode_failed"),
            };
        }
        let mut result = match serde_json::to_value(doc.value()) {
            Ok(v) => v,
            Err(_) => return error(500, "decode_failed"),
        };
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
        response
            .headers_mut()
            .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
        return response;
    }
    if ![Method::PUT, Method::PATCH, Method::DELETE].contains(&method) {
        return error(405, "method_not_allowed");
    }
    if yaml && method != Method::PUT {
        return error(405, "method_not_allowed");
    }
    let before = doc.clone();
    if method == Method::DELETE {
        if parts.is_empty() {
            return error(400, "cannot_delete_config");
        }
        if !doc.delete(&parts) {
            return error(404, "not_found");
        }
    } else {
        let update = if yaml {
            serde_yaml_ng::from_slice(body)
        } else {
            let json = match serde_json::from_slice::<Value>(body) {
                Ok(v) => v,
                Err(_) => return error(400, "invalid_json"),
            };
            serde_yaml_ng::to_value(json)
        };
        let value = match update {
            Ok(v) => v,
            Err(_) => return error(400, "invalid_body"),
        };
        if parts.is_empty() && !value.is_mapping() {
            return error(400, "config_must_be_object");
        }
        if doc.update(&parts, value, method == Method::PATCH).is_err() {
            return error(400, "invalid_path");
        }
    }
    for field in [
        "credentials/concurrency/lifecycle-config-revision",
        "credentials/concurrency/observation-barrier-revision",
        "plugins/auth-revision",
    ] {
        let parts: Vec<_> = field.split('/').collect();
        if doc.get(&parts) != before.get(&parts) {
            return error(400, "read_only_field");
        }
    }
    // Secret round-trips need endpoint matching, so reject TURN mutations rather than
    // accidentally clearing/redelivering somebody else's password.
    let turn = ["oauth", "providers", "codex", "live-media-relay", "ice-servers"];
    if !yaml && doc.get(&turn) != before.get(&turn) {
        return error(501, "not_implemented");
    }
    let text = match doc.yaml() {
        Ok(v) => v,
        Err(_) => return error(422, "invalid_config"),
    };
    if Config::parse(&text).is_err() {
        return error(422, "invalid_config");
    }
    if cpa_core::config::validate_config_fields(doc.value(), true).is_err() {
        return error(400, "invalid_config");
    }
    if let Some(secret) = doc
        .get(&["management", "secret-key"])
        .and_then(serde_yaml_ng::Value::as_str)
        && !secret.is_empty()
        && !is_bcrypt(secret)
    {
        let hash = match bcrypt::hash(secret, 10) {
            Ok(v) => v,
            Err(_) => return error(422, "invalid_config"),
        };
        if doc.update(&["management", "secret-key"], hash.into(), false).is_err() {
            return error(422, "invalid_config");
        }
    }
    let basis = if yaml {
        match std::str::from_utf8(body) {
            Ok(v) => v,
            Err(_) => return error(400, "invalid_body"),
        }
    } else {
        &original
    };
    let text = match doc.render_preserving(basis) {
        Ok(v) => v,
        Err(_) => return error(422, "invalid_config"),
    };
    let cfg = match Config::parse(&text) {
        Ok(v) => v,
        Err(_) => return error(422, "invalid_config"),
    };
    if ConfigDocument::write(&state.path, &text).is_err() {
        return error(500, "write_failed");
    }
    state.publish(cfg, None);
    json(StatusCode::OK, &json!({"status":"ok", "config-version":8}))
}

async fn legacy_yaml(State(state): State<Arc<Management>>) -> Response {
    match tokio::fs::read(&state.path).await {
        Ok(bytes) => ([(header::CONTENT_TYPE, "application/yaml; charset=utf-8")], bytes).into_response(),
        Err(_) => error(404, "not_found"),
    }
}

async fn credentials(State(state): State<Arc<Management>>) -> Response {
    // ponytail: interim file-only inventory; the full Go projection (auth_index,
    // cooldowns, counters, filters, pagination) replaces this in the credentials pass.
    // Go lists only file-backed and runtime-only auths, never config API keys.
    let files: Vec<_> = state
        .rt
        .store()
        .snapshot()
        .iter()
        .filter(|c| matches!(c.source, Source::File(_)))
        .map(|c| {
            json!({
                "id":c.id, "name":c.id, "auth_index":credentials::auth_index(c), "provider":c.provider,
                "type":c.provider, "email":c.str("email").unwrap_or_default(), "label":c.label,
                "disabled":c.disabled, "status":if c.disabled {"disabled"} else {"active"},
                "runtime_only":false, "unavailable":false, "cooldowns":null
            })
        })
        .collect();
    json(StatusCode::OK, &json!({"files": files}))
}

async fn download(
    State(state): State<Arc<Management>>,
    axum::extract::Query(query): axum::extract::Query<HashMap<String, String>>,
) -> Response {
    let Some(name) = query.get("name") else {
        return error(400, "invalid name");
    };
    // Lookup the store source rather than joining an untrusted filename to auth-dir.
    let Some(c) = state.rt.store().get(name) else {
        return error(404, "not_found");
    };
    let Source::File(path) = &c.source else {
        return error(404, "not_found");
    };
    match tokio::fs::read(path).await {
        Ok(bytes) => ([(header::CONTENT_TYPE, "application/json")], bytes).into_response(),
        Err(_) => error(404, "not_found"),
    }
}

async fn status(State(state): State<Arc<Management>>, body: Bytes) -> Response {
    let value: Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(_) => return error(400, "invalid request body"),
    };
    let name = value.get("name").and_then(Value::as_str).unwrap_or_default().trim();
    if name.is_empty() {
        return error(400, "name is required");
    }
    let Some(disabled) = value.get("disabled").and_then(Value::as_bool) else {
        return error(400, "disabled is required");
    };
    let Some(credential) = state
        .rt
        .store()
        .get(name)
        .filter(|c| matches!(c.source, Source::File(_)))
    else {
        return error(404, "auth file not found");
    };
    let id = credential.id.clone();
    let patch = MetadataPatch {
        set: serde_json::Map::from_iter([("disabled".into(), disabled.into())]),
        remove: vec![],
    };
    let result = tokio::task::spawn_blocking(move || {
        let _guard = state.disk.lock().unwrap_or_else(PoisonError::into_inner);
        state.rt.store().apply_patch(&id, credential.revision, &patch)
    })
    .await;
    match result {
        Ok(Ok(_)) => json(StatusCode::OK, &json!({"status":"ok", "disabled":disabled})),
        Ok(Err(crate::runtime::PatchError::Stale { .. })) => error(409, "stale credential"),
        _ => error(500, "write_failed"),
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
