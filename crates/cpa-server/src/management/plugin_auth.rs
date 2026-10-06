//! The plugin host's auth manager (Go `pluginHost.SetAuthManager(authManager)` in
//! internal/api/server.go): `host.auth.*` read the credential store, saves go through
//! the auth-file pipeline uploads use, and `host.affinity.lookup` observes the
//! scheduler's session bindings (Go `Manager.LookupSessionAffinity`).
//!
//! ponytail: Go's raw `Auth.Status` is approximated from the store as the auth-files
//! view does: `disabled`, `error` while a credential-wide cooldown runs, else
//! `active`; created and updated times are the file's modification time.
//! ponytail: an overwrite that changes a credential's type gets that type's auth
//! index at once; Go's `Update` keeps the old index until the next restart.
//! ponytail: a binding made for a selection across providers is keyed by its provider
//! list, where Go uses one `mixed` key, so two such selections can both hold a
//! session that Go would have rebound (reported as `ambiguous` here).

use std::collections::{BTreeMap, BTreeSet};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::{Arc, PoisonError, Weak};
use std::time::SystemTime;

use cpa_core::config::credentials;
use cpa_core::credential::Credential;
use cpa_plugin::gojson::GoTime;
use cpa_plugin::hostauth::{AuthManager, HostAuth};
use serde_json::Value;

use super::Management;
use super::auth_files::{auth_kind, file_name, parse_time, path_of, recent_requests, status_message};

/// Go `cliproxysession.CandidateSessionPrefixes`.
const CANDIDATE_SESSION_PREFIXES: [&str; 18] = [
    "lcp:v1:",
    "lcp:",
    "codex:",
    "claude:",
    "header:",
    "session:",
    "affinity:",
    "slot:",
    "task:",
    "conv:",
    "thread:",
    "clientreq:",
    "geminicache:",
    "pck:",
    "user:",
    "execution:",
    "agy:",
    "derived:",
];

/// Gives the runtime's plugin host this management state as its auth manager.
pub(crate) fn attach(state: &Arc<Management>) {
    state
        .rt
        .plugins()
        .set_auth_manager(Some(Arc::new(PluginAuthManager(Arc::downgrade(state)))));
}

/// Held weakly: the runtime owns the plugin host, and management owns the runtime.
struct PluginAuthManager(Weak<Management>);

impl AuthManager for PluginAuthManager {
    fn list(&self) -> Vec<HostAuth> {
        let Some(state) = self.0.upgrade() else {
            return Vec::new();
        };
        state
            .rt
            .store()
            .snapshot()
            .iter()
            .map(|c| host_auth(&state, c))
            .collect()
    }

    fn get_by_id(&self, id: &str) -> Option<HostAuth> {
        let state = self.0.upgrade()?;
        state.rt.store().get(id).map(|c| host_auth(&state, &c))
    }

    /// Under the disk lock (the auth-file writes and reloads lock): the host writes the
    /// file, the file store rewrites it in its canonical form, and the credential is
    /// registered or replaced alone, keeping its runtime state by ID. Nothing else is
    /// reloaded and the config is not republished.
    fn save(&self, auth: HostAuth, write: &mut dyn FnMut() -> Result<(), String>) -> Result<(), String> {
        let Some(state) = self.0.upgrade() else {
            return write();
        };
        let _disk = state.disk.lock().unwrap_or_else(PoisonError::into_inner);
        write()?;
        let path = PathBuf::from(auth.attributes.get("path").cloned().unwrap_or_default());
        let bytes = persist(&path, &auth)?;
        let cfg = state.rt.config();
        let credential = match credentials::from_file(&cfg, &cfg.auth_dir, &path, &bytes) {
            Ok(Some(c)) => {
                state
                    .fallbacks
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .remove(&path);
                c
            }
            // Go registers what its synthesizer would skip; uploads keep such files the
            // same way (see `credentials::upload_fallback`).
            Ok(None) => match credentials::upload_fallback(&cfg.auth_dir, &path, &bytes) {
                Some(c) => {
                    state
                        .fallbacks
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner)
                        .insert(path.clone());
                    c
                }
                None => return Ok(()),
            },
            Err(e) => return Err(format!("register auth: {e}")),
        };
        state.rt.store().upsert(credential);
        Ok(())
    }

    /// Go `LookupSessionAffinity` with `SessionAffinitySelector.LookupAffinity`.
    // ponytail: Go also answers `unsupported` while a plugin scheduler is active and
    // consults the LCP session matcher; neither exists here yet.
    fn lookup_session_affinity(&self, provider: &str, model: &str, session_id: &str) -> (String, Option<HostAuth>) {
        let status = |s: &str| (s.to_owned(), None);
        let Some(state) = self.0.upgrade() else {
            return status("unsupported");
        };
        if !state.rt.policy().session_affinity {
            return status("unsupported");
        }
        let model_key = crate::scheduler::canonical_model(model).to_owned();
        let model_key = if model_key.is_empty() {
            model.to_owned()
        } else {
            model_key
        };
        let candidates: Vec<String> = if CANDIDATE_SESSION_PREFIXES.iter().any(|p| session_id.starts_with(p)) {
            vec![session_id.to_owned()]
        } else {
            std::iter::once(session_id.to_owned())
                .chain(CANDIDATE_SESSION_PREFIXES.iter().map(|p| format!("{p}{session_id}")))
                .collect()
        };
        let bounded: Vec<String> = candidates
            .iter()
            .map(|c| cpa_common::session::bound_session_identity(c))
            .collect();
        let store = state.rt.store();
        let snapshot = store.snapshot();
        // Bindings are keyed by scheduling key (an OpenAI-compatible credential's
        // `provider_key`), which Go's `Auth.Provider` holds.
        let provider_of = |id: &str| {
            snapshot
                .iter()
                .find(|c| c.id == id)
                .map(|c| crate::registry::provider_key(c))
        };
        // Go keys bindings by provider, or `mixed` for a selection across providers;
        // here a mixed selection's scope lists its providers.
        let mixed = provider == "mixed";
        let found: BTreeSet<String> = store
            .affinity_bound(|(scope, key_model, session)| {
                let scope_ok = if mixed {
                    scope.contains(',')
                } else {
                    scope == provider || scope.contains(',')
                };
                scope_ok && *key_model == model_key && bounded.contains(session)
            })
            .into_iter()
            .filter(|id| mixed || provider_of(id).as_deref() == Some(provider))
            .collect();
        let auth_id = match found.len() {
            0 => return status("unbound"),
            1 => found.into_iter().next().unwrap_or_default(),
            _ => return status("ambiguous"),
        };
        match snapshot.iter().find(|c| c.id == auth_id) {
            Some(c) if mixed || crate::registry::provider_key(c) == provider => {
                ("bound".into(), Some(host_auth(&state, c)))
            }
            _ => status("unbound"),
        }
    }
}

/// Go `FileTokenStore.Save` for a saved auth: the metadata with the auth's `disabled`
/// flag, marshalled, replacing the file unless it already holds the same JSON.
/// Returns what the file holds.
fn persist(path: &Path, auth: &HostAuth) -> Result<Vec<u8>, String> {
    let mut meta: BTreeMap<String, Value> = auth.metadata.clone().into_iter().collect();
    meta.insert("disabled".into(), auth.disabled.into());
    let raw = cpa_plugin::gojson::to_vec(&meta);
    let existing = std::fs::read(path).map_err(|e| format!("auth filestore: read existing failed: {e}"))?;
    if json_equal(&existing, &raw) {
        return Ok(existing);
    }
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .truncate(true)
        .open(path)
        .map_err(|e| format!("auth filestore: open existing failed: {e}"))?;
    file.write_all(&raw)
        .map_err(|e| format!("auth filestore: write existing failed: {e}"))?;
    Ok(raw)
}

/// Go `jsonEqual`: both decode, and the decoded values are equal (numbers as float64).
fn json_equal(a: &[u8], b: &[u8]) -> bool {
    fn same(a: &Value, b: &Value) -> bool {
        match (a, b) {
            (Value::Number(x), Value::Number(y)) => x.as_f64() == y.as_f64(),
            (Value::Array(x), Value::Array(y)) => x.len() == y.len() && x.iter().zip(y).all(|(x, y)| same(x, y)),
            (Value::Object(x), Value::Object(y)) => {
                x.len() == y.len() && x.iter().all(|(k, v)| y.get(k).is_some_and(|w| same(v, w)))
            }
            _ => a == b,
        }
    }
    match (serde_json::from_slice::<Value>(a), serde_json::from_slice::<Value>(b)) {
        (Ok(a), Ok(b)) => same(&a, &b),
        _ => false,
    }
}

fn go_time(t: Option<SystemTime>) -> GoTime {
    GoTime(t.map(|t| chrono::DateTime::<chrono::Local>::from(t).fixed_offset()))
}

/// A stored credential as Go's `coreauth.Auth` presents it to the plugin host.
fn host_auth(state: &Management, c: &Credential) -> HostAuth {
    let store = state.rt.store();
    let cooldown = store
        .cooldowns(&c.id)
        .into_iter()
        .find(|cd| cd.model.is_empty() && !cd.remaining.is_zero());
    let (status, unavailable) = if c.disabled {
        ("disabled", false)
    } else if cooldown.is_some() {
        ("error", true)
    } else {
        ("active", false)
    };
    let mut attributes = c.attributes.clone();
    let path = path_of(c);
    if let Some(path) = path {
        let path = std::path::absolute(path).unwrap_or_else(|_| path.to_owned());
        let path = path.display().to_string();
        attributes.entry("path".into()).or_insert_with(|| path.clone());
        attributes.entry("source".into()).or_insert(path);
    }
    let activity = store.activity(&c.id);
    let recent = match recent_requests(&activity) {
        Value::Array(buckets) => buckets
            .iter()
            .map(|b| {
                (
                    b["time"].as_str().unwrap_or_default().to_owned(),
                    b["success"].as_i64().unwrap_or_default(),
                    b["failed"].as_i64().unwrap_or_default(),
                )
            })
            .collect(),
        _ => Vec::new(),
    };
    let modified = path
        .and_then(|p| std::fs::metadata(p).ok())
        .and_then(|m| m.modified().ok());
    let last_refresh = ["last_refresh", "lastRefresh", "last_refreshed_at", "lastRefreshedAt"]
        .iter()
        .find_map(|key| c.metadata.get(*key).and_then(parse_time));
    let email = c
        .metadata
        .get("email")
        .and_then(Value::as_str)
        .map(str::trim)
        .unwrap_or_default();
    let (account_type, account) = match auth_kind(c) {
        Some("oauth") => ("oauth", email.to_owned()),
        Some(_) => (
            "api_key",
            c.attributes
                .get("api_key")
                .map(|k| k.trim().to_owned())
                .unwrap_or_default(),
        ),
        None => ("", String::new()),
    };
    HostAuth {
        id: c.id.clone(),
        index: credentials::auth_index(c),
        provider: c.provider.clone(),
        file_name: file_name(c).unwrap_or_default(),
        label: c.label.clone(),
        status: status.into(),
        status_message: status_message(state, c).into(),
        disabled: c.disabled,
        unavailable,
        attributes,
        metadata: c.metadata.clone(),
        success: activity.success as i64,
        failed: activity.failed as i64,
        recent_requests: recent,
        created_at: go_time(modified),
        updated_at: go_time(modified),
        last_refreshed_at: go_time(last_refresh),
        next_retry_after: go_time(cooldown.map(|cd| SystemTime::now() + cd.remaining)),
        account_type: account_type.into(),
        account,
    }
}
