//! Plugin OAuth logins (internal/api/handlers/management/auth_files_oauth_callback.go
//! `ServePluginAuthURL`, the plugin branch of `GetAuthStatus` and
//! `savePluginLoginRecords`): a plugin auth provider starts a login for
//! `/v0/management/<provider>-auth-url` (through NoRoute) or
//! `/v8/management/oauth/auth-url?provider=<provider>`, the callback is written where
//! the plugin reads it, and polling the status saves the auths it returns.

use std::path::Path;
use std::sync::Arc;

use axum::http::StatusCode;
use axum::response::Response;
use cpa_plugin::auth::PluginAuth;
use cpa_plugin::gojson::Metadata;
use serde_json::{Value, json};

use super::Management;
use super::plugins::{go_query, map_json};

/// Go `NormalizePluginOAuthCallbackProvider`: lowercase letters, digits and `-`.
pub(super) fn normalize_provider(provider: &str) -> Option<String> {
    let p = provider.trim().to_lowercase();
    (!p.is_empty()
        && p.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-'))
    .then_some(p)
}

fn error(status: StatusCode, message: &str) -> Response {
    map_json(status, &json!({"error": message}))
}

/// Go `managementCallbackURL`.
fn callback_url(state: &Management, path: &str) -> Option<String> {
    let cfg = state.rt.config();
    if cfg.port == 0 {
        return None;
    }
    let tls = cfg
        .document
        .get("server")
        .and_then(|s| s.get("tls"))
        .and_then(|t| t.get("enable"))
        .and_then(serde_yaml_ng::Value::as_bool)
        .unwrap_or(false);
    Some(format!(
        "{}://127.0.0.1:{}{path}",
        if tls { "https" } else { "http" },
        cfg.port
    ))
}

/// Go `queryValuesToMetadata`: one value as a string, several as a list.
fn query_metadata(pairs: Vec<(String, String)>) -> Metadata {
    let mut grouped: Vec<(String, Vec<String>)> = Vec::new();
    for (k, v) in pairs {
        match grouped.iter_mut().find(|(key, _)| *key == k) {
            Some((_, values)) => values.push(v),
            None => grouped.push((k, vec![v])),
        }
    }
    grouped
        .into_iter()
        .map(|(k, mut v)| {
            let value = if v.len() == 1 {
                Value::String(v.remove(0))
            } else {
                json!(v)
            };
            (k, value)
        })
        .collect()
}

/// Go `ServePluginAuthURL`: `None` when no plugin auth provider handles the path
/// (`path` is the decoded request path).
pub(super) async fn serve_auth_url(state: &Arc<Management>, path: &str, raw_query: &str) -> Option<Response> {
    let mut query = go_query(raw_query);
    let v8 = path.trim() == "/v8/management/oauth/auth-url";
    let provider = if v8 {
        query
            .iter()
            .find(|(k, _)| k == "provider")
            .map(|(_, v)| v.clone())
            .unwrap_or_default()
    } else {
        let rest = path.trim().strip_prefix("/v0/management/")?;
        rest.strip_suffix("-auth-url")?.to_owned()
    };
    let provider = normalize_provider(&provider)?;
    let host = state.rt.plugins();
    if !host.has_auth_provider(&provider) {
        return None;
    }
    let callback_path = if v8 {
        query.retain(|(k, _)| k != "provider");
        "/v8/management/oauth/callback"
    } else {
        "/v0/management/oauth-callback"
    };
    let Some(base_url) = callback_url(state, callback_path) else {
        tracing::error!("failed to compute plugin auth callback URL: server port is not configured");
        return Some(error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "failed to generate authorization url",
        ));
    };
    let resp = match host
        .start_login(&provider, &base_url, query_metadata(query), &Default::default())
        .await
    {
        Ok(Some(resp)) => resp,
        Ok(None) => return None,
        Err(e) => {
            tracing::error!(error = %e, "failed to start plugin auth login");
            return Some(error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "failed to generate authorization url",
            ));
        }
    };
    let sid = resp.state.trim();
    if sid.is_empty() || !super::oauth::valid_state(sid) {
        tracing::error!(provider = %provider, "plugin auth provider returned an empty or invalid state");
        return Some(error(StatusCode::BAD_GATEWAY, "invalid oauth state"));
    }
    if !state.oauth.register_plugin(sid, &provider, resp.metadata.clone()) {
        tracing::error!(provider = %provider, "failed to register plugin oauth session");
        return Some(error(StatusCode::BAD_GATEWAY, "failed to generate authorization url"));
    }
    Some(map_json(
        StatusCode::OK,
        &json!({"status": "ok", "url": resp.url, "state": sid}),
    ))
}

/// The plugin branch of Go `GetAuthStatus`: polls the plugin, saving the auths a
/// successful login returns. `None` when the plugin does not handle the poll.
pub(super) async fn poll(state: &Arc<Management>, sid: &str, provider: &str, metadata: Metadata) -> Option<Response> {
    let host = state.rt.plugins();
    let status = |s: &str| map_json(StatusCode::OK, &json!({"status": s}));
    let failed = |message: &str| {
        state.oauth.set_error(sid, message);
        map_json(StatusCode::OK, &json!({"status": "error", "error": message}))
    };
    let resp = match host.poll_login(provider, sid, metadata, &Default::default()).await {
        Ok(Some(resp)) => resp,
        Ok(None) => return None,
        Err(e) => {
            let message = match e.to_string().trim() {
                "" => "Authentication failed".to_owned(),
                m => m.to_owned(),
            };
            return Some(failed(&message));
        }
    };
    Some(match resp.status.as_str() {
        "error" => failed(match resp.message.trim() {
            "" => "Authentication failed",
            m => m,
        }),
        "success" => {
            // Go `pluginLoginPollAuths`: `Auths`, else the single `Auth`.
            let data = if resp.auths.is_empty() {
                vec![resp.auth]
            } else {
                resp.auths
            };
            let auth_dir = host.host_config_summary().auth_dir;
            let records: Option<Vec<PluginAuth>> = data
                .into_iter()
                .map(|d| PluginAuth::from_auth_data(d, "", "", &auth_dir))
                .collect();
            let Some(records) = records.filter(|r| !r.is_empty()) else {
                return Some(failed("Authentication failed"));
            };
            let saved = {
                let state = state.clone();
                tokio::task::spawn_blocking(move || save_records(&state, records)).await
            };
            match saved {
                Ok(Ok(())) => {
                    state.oauth.complete(sid);
                    status("ok")
                }
                Ok(Err(e)) => {
                    tracing::error!(error = %e, provider = %provider, "failed to save plugin auth tokens");
                    failed("Failed to save authentication tokens")
                }
                Err(_) => failed("Failed to save authentication tokens"),
            }
        }
        // "", "pending" and anything else keep the client waiting.
        _ => status("wait"),
    })
}

/// Go `savePluginLoginRecords`: each auth saved as `saveTokenRecord` saves it (existing
/// file metadata merged, written with creation intent); on a failure the files saved
/// so far are removed.
// ponytail: Go's auth-manager metadata fallback, legacy Claude credential migration and
// post-auth hooks (SDK embedders only) are not ported; the watcher loads the new files.
fn save_records(state: &Management, records: Vec<PluginAuth>) -> Result<(), String> {
    let _guard = state.disk.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let auth_dir = state.rt.config().auth_dir.clone();
    let mut saved: Vec<String> = Vec::new();
    for mut record in records {
        // Go `mergeExistingAuthFileMetadata`.
        let target = if record.file_name.is_empty() {
            &record.id
        } else {
            &record.file_name
        };
        if !auth_dir.as_os_str().is_empty()
            && !target.is_empty()
            && let Ok(raw) = std::fs::read(go_join(&auth_dir, target))
            && let Ok(Value::Object(existing)) = serde_json::from_slice::<Value>(&raw)
        {
            record.merge_existing(&existing);
        }
        match record.save_file(&auth_dir.to_string_lossy(), true) {
            Ok(path) => {
                if !path.trim().is_empty() {
                    saved.push(path);
                }
            }
            Err(e) => {
                // Go `rollbackSavedTokenRecords`, newest first.
                for path in saved.iter().rev() {
                    let _ = std::fs::remove_file(path);
                    if let Err(e) = state.store_delete(Path::new(path)) {
                        tracing::warn!(error = %e, path = %path, "failed to roll back plugin auth token");
                    }
                }
                return Err(e);
            }
        }
    }
    Ok(())
}

/// Go `filepath.Join(dir, name)`: lexical, so an absolute name still lands under
/// `dir`.
fn go_join(dir: &Path, name: &str) -> std::path::PathBuf {
    let joined = format!("{}/{name}", dir.to_string_lossy());
    cpa_plugin::platform::clean(Path::new(&joined))
}

/// Go `writeOAuthCallbackFile` for a plugin session: `.oauth-<provider>-<state>.oauth`
/// in the auth directory, published atomically, where the plugin's poll reads it.
pub(super) fn write_callback_file(
    auth_dir: &Path,
    provider: &str,
    sid: &str,
    code: &str,
    error: &str,
) -> Result<(), String> {
    use std::io::Write as _;
    if auth_dir.as_os_str().is_empty() {
        return Err("auth dir is empty".into());
    }
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
    builder
        .create(auth_dir)
        .map_err(|e| format!("create oauth callback dir: {e}"))?;
    let payload = serde_json::to_vec(&json!({"code": code.trim(), "state": sid.trim(), "error": error.trim()}))
        .map_err(|e| e.to_string())?;
    let target = auth_dir.join(format!(".oauth-{provider}-{}.oauth", sid.trim()));
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    let mut attempt = 0u32;
    let (tmp_path, mut tmp) = loop {
        let candidate = auth_dir.join(format!(".oauth-callback-{}{attempt}", std::process::id()));
        match options.open(&candidate) {
            Ok(file) => break (candidate, file),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists && attempt < 10_000 => attempt += 1,
            Err(e) => return Err(format!("create oauth callback file: {e}")),
        }
    };
    let result = tmp
        .write_all(&payload)
        .map_err(|e| format!("write oauth callback file: {e}"))
        .and_then(|()| {
            drop(tmp);
            std::fs::rename(&tmp_path, &target).map_err(|e| format!("publish oauth callback file: {e}"))
        });
    let _ = std::fs::remove_file(&tmp_path);
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn existing_metadata_is_read_where_go_joins() {
        let dir = Path::new("/auth");
        assert_eq!(go_join(dir, "a.json"), Path::new("/auth/a.json"));
        // Go joins lexically: an absolute file name is looked up under the auth dir.
        assert_eq!(go_join(dir, "/tmp/a.json"), Path::new("/auth/tmp/a.json"));
        assert_eq!(go_join(dir, "../x.json"), Path::new("/x.json"));
        assert_eq!(go_join(dir, "sub/./b.json"), Path::new("/auth/sub/b.json"));
    }

    #[test]
    fn query_metadata_groups_repeated_keys() {
        let pairs = vec![
            ("b".to_owned(), "1".to_owned()),
            ("a".to_owned(), "x".to_owned()),
            ("b".to_owned(), "2".to_owned()),
        ];
        let m = query_metadata(pairs);
        assert_eq!(m["a"], serde_json::json!("x"));
        assert_eq!(m["b"], serde_json::json!(["1", "2"]));
    }
}
