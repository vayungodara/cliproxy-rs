//! Management API (v8, plus the few legacy v0 reads the dashboard predates).
//!
//! Routing follows gin: an unknown path or an unregistered method on a known path is a
//! bare 404 that never reaches authentication. Access rules live in [`access`].
use std::path::PathBuf;
use std::sync::{Arc, Mutex, PoisonError};

use axum::body::Bytes;
use axum::extract::{OriginalUri, State};
use axum::handler::Handler;
use axum::http::{HeaderValue, Method, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{MethodRouter, any, get};
use axum::{Router, middleware};
use cpa_core::config::{Config, ConfigDocument, archive_comments, credentials, is_bcrypt};
use cpa_core::credential::{Credential, Source};
use serde_json::{Value, json};

use crate::Runtime;
use crate::scheduler::{ErrorRule, Policy};

mod access;
mod auth_files;
mod multipart;
pub use access::cors;

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
            fallbacks: Mutex::default(),
            disabled_via_api: Mutex::default(),
        })
    }

    /// Publishes a config and everything Go derives from it on reload: scheduler
    /// policy, management availability, and the credential set (auth-dir files plus
    /// config API keys). `files` replaces the auth-dir scan when the caller already
    /// has a reconciled list. Callers hold `disk`. Infallible on purpose: access
    /// settings of a valid config (a rotated or removed secret) always take effect.
    pub(crate) fn publish(&self, cfg: Config, files: Option<Vec<Credential>>) {
        let mut all = files.unwrap_or_else(|| credentials::from_auth_dir(&cfg));
        // Fallbacks follow their file: gone with it, replaced once a synthesizer
        // claims it, otherwise rebuilt from its current content.
        let mut fallbacks = self.fallbacks.lock().unwrap_or_else(PoisonError::into_inner);
        fallbacks.retain(|path| {
            let Ok(data) = std::fs::read(path) else { return false };
            if all.iter().any(|c| matches!(&c.source, Source::File(p) if p == path)) {
                return false;
            }
            match credentials::upload_fallback(&cfg.auth_dir, path, &data) {
                Some(c) => {
                    all.push(c);
                    true
                }
                None => false,
            }
        });
        drop(fallbacks);
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
        .route(&format!("{v0}/config.yaml"), methods().get(guarded!(s, legacy_yaml)));
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
    router
        .route(
            &format!("{v8}/routing/cooldown/reset"),
            methods().post(guarded!(s, auth_files::cooldown_reset)),
        )
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

/// Go `Handler.ConfigV8`: every call reads the file and migrates it in memory (legacy
/// layout to v8, unknown sections archived as comments); only successful mutations
/// persist. The write keeps untouched text byte-stable instead of Go's re-encoding.
fn config_sync(state: &Management, path: &str, method: Method, body: &[u8]) -> Response {
    let _guard = state.disk.lock().unwrap_or_else(PoisonError::into_inner);
    let original = match std::fs::read_to_string(&state.path) {
        Ok(v) => v,
        Err(_) => return error(500, "read_failed"),
    };
    let mut doc = match ConfigDocument::parse(&original) {
        Ok(v) => v,
        Err(e) => return invalid_config(StatusCode::INTERNAL_SERVER_ERROR, e),
    };
    let basis = doc.migrated_text(&original).unwrap_or_else(|| original.clone());
    let archived = doc.archive_unknown();
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
            return match doc.render_preserving(&basis) {
                Ok(text) => (
                    [
                        (header::CONTENT_TYPE, "application/yaml; charset=utf-8"),
                        (header::CACHE_CONTROL, "no-store"),
                    ],
                    text + &archive_comments(&archived),
                )
                    .into_response(),
                Err(_) => error(500, "decode_failed"),
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
    let before = doc.clone();
    let mut written = None;
    if method == Method::DELETE {
        if parts.is_empty() {
            return error(400, "cannot_delete_config");
        }
        if !doc.delete(&parts) {
            return error(404, "not_found");
        }
    } else {
        if !yaml && serde_json::from_slice::<serde::de::IgnoredAny>(body).is_err() {
            return error(400, "invalid_json");
        }
        // yaml.v3 yields no document for an empty or comment-only body (an explicit
        // `null` is a document).
        let has_document = String::from_utf8_lossy(body).lines().any(|l| {
            let t = l.trim();
            !t.is_empty() && !t.starts_with('#') && t != "---" && t != "..."
        });
        let value = match serde_yaml_ng::from_slice::<serde_yaml_ng::Value>(body) {
            Ok(v) if has_document => v,
            _ => return error(400, "invalid_body"),
        };
        if parts.is_empty() && !value.is_mapping() {
            return error(400, "config_must_be_object");
        }
        if doc.update(&parts, value.clone(), method == Method::PATCH).is_err() {
            return error(400, "invalid_path");
        }
        if !yaml {
            doc.preserve_turn_secrets(&before);
        }
        written = Some(value);
    }
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
    if let Err(e) = Config::parse(&text) {
        return invalid_config(StatusCode::UNPROCESSABLE_ENTITY, e);
    }
    if let Err(e) = cpa_core::config::validate_config_fields(doc.value(), true) {
        return invalid_config(StatusCode::BAD_REQUEST, e);
    }
    // Go validates the raw candidate, then its saver persists typed values; projecting
    // only after validation keeps malformed input from being sanitized into success.
    if let Some(written) = &written {
        doc.typed_projection(&parts, written, method == Method::PATCH);
    }
    if let Some(secret) = doc
        .get(&["management", "secret-key"])
        .and_then(serde_yaml_ng::Value::as_str)
        && !secret.is_empty()
        && !is_bcrypt(secret)
    {
        let hash = match bcrypt::hash(secret, bcrypt::DEFAULT_COST) {
            Ok(v) => v,
            Err(e) => return invalid_config(StatusCode::UNPROCESSABLE_ENTITY, e),
        };
        if doc.update(&["management", "secret-key"], hash.into(), false).is_err() {
            return error(422, "invalid_config");
        }
    }
    let basis = if yaml {
        match std::str::from_utf8(body) {
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
    json(StatusCode::OK, &json!({"config-version": 8, "status": "ok"}))
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
