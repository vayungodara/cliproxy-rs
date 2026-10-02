//! Management API over persisted config and the shared credential store.
use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use axum::body::Bytes;
use axum::extract::{ConnectInfo, OriginalUri, Request, State};
use axum::http::{Method, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::routing::{any, get};
use axum::{Json, Router, middleware};
use cpa_core::config::{Config, ConfigDocument, is_bcrypt};
use cpa_core::credential::{MetadataPatch, Source};
use serde_json::{Value, json};
use subtle::ConstantTimeEq;

use crate::Runtime;

pub struct Management {
    pub(crate) rt: Arc<Runtime>,
    pub(crate) path: PathBuf,
    // ponytail: one lock for config and auth disk operations; split only if management
    // throughput matters. The watcher shares it, preventing stale disk publication.
    pub(crate) disk: Mutex<()>,
    failures: Mutex<HashMap<std::net::IpAddr, (u8, Instant)>>,
    env_secret: String,
}

impl Management {
    pub fn new(rt: Arc<Runtime>, path: PathBuf) -> Arc<Self> {
        Arc::new(Self {
            rt,
            path,
            disk: Mutex::new(()),
            failures: Mutex::new(HashMap::new()),
            env_secret: std::env::var("MANAGEMENT_PASSWORD")
                .unwrap_or_default()
                .trim()
                .to_owned(),
        })
    }
}

pub fn router(state: Arc<Management>) -> Router {
    let api = Router::new()
        .route("/config", any(config))
        .route("/config.yaml", any(config))
        .route("/config/{*path}", any(config))
        .route("/credentials", get(credentials))
        .route("/credentials/download", get(download))
        .route("/credentials/status", axum::routing::patch(status))
        .fallback(|| async { error(501, "not_implemented") })
        .layer(middleware::from_fn_with_state(state.clone(), authenticate));
    let legacy = Router::new()
        .route("/config.yaml", get(legacy_yaml))
        .route("/auth-files", get(credentials))
        .route("/auth-files/download", get(download))
        .route("/auth-files/status", axum::routing::patch(status))
        .fallback(|| async { error(501, "not_implemented") })
        .layer(middleware::from_fn_with_state(state.clone(), authenticate));
    Router::new()
        .nest("/v8/management", api)
        .nest("/v0/management", legacy)
        .route("/management.html", get(panel))
        .route("/assets/{*path}", get(panel))
        .route("/fonts/{*path}", get(panel))
        .route("/favicon.svg", get(panel))
        .with_state(state)
}

fn error(code: u16, message: &str) -> Response {
    (StatusCode::from_u16(code).unwrap(), Json(json!({"error": message}))).into_response()
}

async fn authenticate(State(state): State<Arc<Management>>, req: Request, next: Next) -> Response {
    let cfg = state.rt.config();
    let key = cfg.management.secret_key.clone();
    if key.is_empty() && state.env_secret.is_empty() {
        return error(404, "not_found");
    }
    let peer = req
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map(|p| p.0.ip().to_canonical());
    // Never trust X-Forwarded-For implicitly. The trusted-proxy port is still pending.
    let local = peer.is_some_and(|ip| ip.is_loopback());
    if !local && !cfg.management.allow_remote && state.env_secret.is_empty() {
        return error(403, "remote management disabled");
    }
    let banned = peer.is_some_and(|ip| {
        state
            .failures
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&ip)
            .is_some_and(|(count, since)| *count >= 5 && since.elapsed() < Duration::from_secs(1800))
    });
    if banned {
        return error(403, "IP banned due to too many failed attempts");
    }
    let authorization = req
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|h| h.to_str().ok())
        .unwrap_or_default();
    let provided = authorization
        .split_once(' ')
        .filter(|(kind, _)| kind.eq_ignore_ascii_case("bearer"))
        .map(|(_, value)| value)
        .unwrap_or(authorization);
    let provided = if provided.is_empty() {
        req.headers()
            .get("X-Management-Key")
            .and_then(|h| h.to_str().ok())
            .unwrap_or_default()
    } else {
        provided
    };
    let provided = provided.to_owned();
    let valid = !provided.is_empty()
        && ((!state.env_secret.is_empty() && bool::from(state.env_secret.as_bytes().ct_eq(provided.as_bytes())))
            || tokio::task::spawn_blocking({
                let provided = provided.clone();
                move || bcrypt::verify(provided, &key).unwrap_or(false)
            })
            .await
            .unwrap_or(false));
    if let Some(ip) = peer {
        let mut failures = state.failures.lock().unwrap_or_else(PoisonError::into_inner);
        if valid {
            failures.remove(&ip);
        } else {
            // Bound stale entries rather than retaining arbitrary remote IPs forever.
            failures.retain(|_, (_, since)| since.elapsed() < Duration::from_secs(3600));
            let attempt = failures.entry(ip).or_insert((0, Instant::now()));
            if attempt.1.elapsed() >= Duration::from_secs(1800) {
                *attempt = (0, Instant::now());
            }
            attempt.0 += 1;
            attempt.1 = Instant::now();
        }
    }
    let mut response = if valid {
        next.run(req).await
    } else {
        error(
            401,
            if provided.is_empty() {
                "missing management key"
            } else {
                "invalid management key"
            },
        )
    };
    response
        .headers_mut()
        .insert("X-CPA-VERSION", "cliproxy-rs/0.1.0".parse().unwrap());
    response
        .headers_mut()
        .insert("X-CPA-SUPPORT-PLUGIN", "false".parse().unwrap());
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, "no-store".parse().unwrap());
    response
}

async fn config(
    State(state): State<Arc<Management>>,
    OriginalUri(uri): OriginalUri,
    method: Method,
    body: Bytes,
) -> Response {
    tokio::task::spawn_blocking(move || config_sync(&state, uri.path(), method, &body))
        .await
        .unwrap_or_else(|_| error(500, "internal_error"))
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
    let suffix = path.strip_prefix("/v8/management/config/").unwrap_or_default();
    let parts: Vec<_> = if suffix.is_empty() {
        vec![]
    } else {
        suffix.split('/').collect()
    };
    if method == Method::GET {
        if yaml {
            return match doc.render_preserving(&original) {
                Ok(text) => ([(header::CONTENT_TYPE, "application/yaml; charset=utf-8")], text).into_response(),
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
        return Json(selected).into_response();
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
    state.rt.publish_config(cfg);
    // Watcher will reconcile a changed auth-dir; it uses the same disk lock.
    Json(json!({"status":"ok", "config-version":8})).into_response()
}

async fn legacy_yaml(State(state): State<Arc<Management>>) -> Response {
    match tokio::fs::read(&state.path).await {
        Ok(bytes) => ([(header::CONTENT_TYPE, "application/yaml; charset=utf-8")], bytes).into_response(),
        Err(_) => error(404, "not_found"),
    }
}

async fn credentials(State(state): State<Arc<Management>>) -> Response {
    // ponytail: file-backed nonpaginated inventory; virtual/config credentials and
    // scheduler cooldown snapshots require the later management parity pass.
    let files: Vec<_> = state
        .rt
        .store()
        .snapshot()
        .iter()
        .map(|c| {
            json!({
                "id":c.id, "name":c.id, "auth_index":c.id, "provider":c.provider,
                "type":c.provider, "email":c.str("email").unwrap_or_default(), "label":c.label,
                "disabled":c.disabled, "status":if c.disabled {"disabled"} else {"active"},
                "runtime_only":matches!(c.source, Source::Config {..}), "unavailable":false,
                "cooldowns":null, "quota_supported":false
            })
        })
        .collect();
    Json(json!({"files": files})).into_response()
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
    let Some(credential) = state.rt.store().get(name) else {
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
        Ok(Ok(_)) => Json(json!({"status":"ok", "disabled":disabled})).into_response(),
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
