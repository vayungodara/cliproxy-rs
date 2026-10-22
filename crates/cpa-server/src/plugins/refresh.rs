//! Refresh of credentials that a plugin auth provider refreshes (Go
//! `executorAdapter.Refresh` and `pluginRefreshCompatExecutor.Refresh`). Built-in
//! providers never take this path: their executors refresh them as before.

use chrono::{DateTime, Utc};
use cpa_common::gostr::GoStr;
use cpa_core::config::Config;
use cpa_core::credential::{Credential, MetadataPatch};
use cpa_core::exec::{ExecError, FailureScope};
use cpa_plugin::hostauth::SavedRefresh;
use serde_json::{Map, Value};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use crate::Runtime;

/// Which Go executor refreshes a credential.
pub(crate) enum Route {
    /// A plugin executor serves it (Go `executorAdapter.Refresh`).
    Executor,
    /// Go's OpenAI-compatible executor wrapped by `pluginRefreshCompatExecutor`;
    /// `identifier` is the wrapped executor's provider key.
    Compat { identifier: String },
}

/// The plugin refresh route of `c`, or `None` when its executor refreshes it.
///
/// Go binds a plugin executor, or registers `pluginRefreshCompatExecutor`, only for
/// providers outside its built-in executor table (`registerExecutorForAuth`): built-in
/// providers return `None` here, compat-named ones included (see
/// docs/DIFFERENCES-FROM-GO.md). For any other provider, a credential a plugin executor
/// serves takes [`Route::Executor`]; the rest are bound to the OpenAI-compatible
/// executor, which Go wraps when a plugin auth provider matches the provider, the
/// `provider_key` or `compat_name` attribute, or the executor's own key
/// (`wrapOpenAICompatIfPluginAuth`). Go asks Home first when Home is enabled
/// (`RefreshAuthViaHome`); that is not ported, so Home mode keeps the compat executor's
/// refresh off.
pub(crate) fn route(rt: &Runtime, c: &Credential, cfg: &Config) -> Option<Route> {
    let provider = c.provider.trim().go_lower();
    if super::BUILTIN.contains(&provider.as_str()) {
        return None;
    }
    if rt.plugin_runtime().executor(c, cfg).is_some() {
        return Some(Route::Executor);
    }
    // Home mode installs the remote dispatcher; the config may not carry `home.enabled`.
    let home = rt.remote_dispatch().is_some()
        || cfg
            .document
            .get("home")
            .and_then(|h| h.get("enabled"))
            .and_then(serde_yaml_ng::Value::as_bool)
            .unwrap_or(false);
    if home {
        return None;
    }
    let attr = |k: &str| c.attributes.get(k).map_or("", |v| v.trim());
    // Go `openAICompatInfoFromAuth`, else the provider (`registerExecutorForAuth`).
    let identifier = if !attr("compat_name").is_empty() {
        let key = if attr("provider_key").is_empty() {
            attr("compat_name")
        } else {
            attr("provider_key")
        };
        cpa_core::config::credentials::openai_compat_provider(key)
    } else if provider == "openai-compatibility" {
        cpa_core::config::credentials::openai_compat_provider(&c.label)
    } else if provider.is_empty() {
        "openai-compatibility".to_owned()
    } else {
        provider.clone()
    };
    let host = rt.plugins();
    [
        provider.as_str(),
        attr("provider_key"),
        attr("compat_name"),
        identifier.as_str(),
    ]
    .iter()
    .any(|key| !key.is_empty() && host.has_auth_provider(key))
    .then_some(Route::Compat { identifier })
}

/// When `c` comes due for a plugin refresh: Go `shouldRefresh` for a provider with no
/// refresh lead, so only a `refresh_interval` schedules one, and never before the
/// credential's `NextRefreshAfter`. The last refresh is the one this process committed
/// for the credential (Go `LastRefreshedAt`), else the credential's own timestamp;
/// without either it is due at once. `None` when it never comes due or no plugin
/// refreshes it.
pub(crate) fn due_at(rt: &Runtime, c: &Credential, cfg: &Config) -> Option<DateTime<Utc>> {
    let interval = cpa_exec::preferred_interval(c)?;
    route(rt, c, cfg)?;
    // Go registers a parsed auth with the plugin's `NextRefreshAfter`, and keeps both
    // fields when the auth is registered again (`prepareCoreAuthForModelRegistration`).
    // The parsed value is taken either way: it seeds this registration only, so a
    // credential registered afresh after its removal waits for a new parse of its file
    // to get one. Taken under the schedules lock, so a save reading this credential's
    // deadlines ([`register_saved`]) sees either the seed or the schedule.
    let mut schedules = lock(&rt.plugin_runtime().schedules);
    let parsed = rt.plugins().take_parsed_next_refresh_after(&c.id);
    let schedule = *schedules.entry(c.id.clone()).or_insert(Schedule {
        last: None,
        next_after: parsed,
    });
    drop(schedules);
    let by_last = schedule
        .last
        .or_else(|| cpa_exec::last_refresh(c))
        .map_or(DateTime::<Utc>::MIN_UTC, |last| {
            last.checked_add_signed(interval).unwrap_or(DateTime::<Utc>::MAX_UTC)
        });
    let by_expiry =
        cpa_exec::expiration(c).map(|expiry| expiry.checked_sub_signed(interval).unwrap_or(DateTime::<Utc>::MIN_UTC));
    let due = by_expiry.map_or(by_last, |at| at.min(by_last));
    Some(schedule.next_after.map_or(due, |after| due.max(after)))
}

/// Go's refresh lifecycle fields of one credential a plugin refreshes. Kept per
/// credential ID, so an edit of the file keeps them, as Go's re-registration does.
#[derive(Clone, Copy, Default)]
pub(crate) struct Schedule {
    /// Go `LastRefreshedAt`.
    last: Option<DateTime<Utc>>,
    /// Go `NextRefreshAfter`.
    next_after: Option<DateTime<Utc>>,
}

/// Credential ID to its [`Schedule`].
pub(crate) type Schedules = Arc<Mutex<HashMap<String, Schedule>>>;

fn lock(schedules: &Schedules) -> std::sync::MutexGuard<'_, HashMap<String, Schedule>> {
    schedules.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Where [`committed`] records plugin refreshes; cloned into the commit, which a
/// cancelled caller cannot stop.
pub(crate) fn schedules(rt: &Runtime) -> Schedules {
    rt.plugin_runtime().schedules.clone()
}

/// Forgets the schedules of credentials that are gone: Go registers them afresh, without
/// the previous `LastRefreshedAt` and `NextRefreshAfter`. A disabled credential keeps
/// its schedule (Go's disable edits the registered auth and keeps both fields).
pub(crate) fn reconcile(rt: &Runtime) {
    let mut schedules = lock(&rt.plugin_runtime().schedules);
    // Read under the lock, so a credential a save registers with its schedule
    // ([`register_saved`]) is never taken for gone.
    let snapshot = rt.store().snapshot();
    let live: std::collections::HashSet<&str> = snapshot.iter().map(|c| c.id.as_str()).collect();
    schedules.retain(|id, _| live.contains(id.as_str()));
}

/// Registers the credentials of an auth save (`register`) with their refresh deadlines
/// (Go `preserveExistingAuthState`, then `upsertAuthRecordsForPath`): an auth
/// replacing a registered one under another ID takes that one's schedule, unset fields
/// included; one replacing itself keeps its own; a new one starts from the
/// `NextRefreshAfter` its plugin parsed (`saved`), or with none. The credentials the
/// save removes lose theirs (Go `Manager.Remove`). The schedules stay locked across the
/// registration, so the refresh loop never sees a saved credential without its own.
pub(crate) fn register_saved(rt: &Runtime, saved: &[SavedRefresh], register: impl FnOnce()) {
    let mut schedules = lock(&rt.plugin_runtime().schedules);
    let ids = |rt: &Runtime| -> std::collections::HashSet<String> {
        rt.store().snapshot().iter().map(|c| c.id.clone()).collect()
    };
    let before = ids(rt);
    let entries: Vec<(String, Schedule)> = saved
        .iter()
        .filter_map(|s| {
            let schedule = match s.donor.as_deref() {
                Some(donor) if donor == s.id => return None,
                // ponytail: a donor never scheduled yet gives the deadline of its file's
                // latest parse, which a reload replaces, where Go gives the one it was
                // registered with (DIFFERENCES-FROM-GO.md). Upgrade: Go's registration rule
                // for plugin refresh state (service_auth.go L427-450) at every register.
                Some(donor) => schedules.get(donor).copied().unwrap_or(Schedule {
                    last: None,
                    next_after: rt.plugins().parsed_next_refresh_after(donor),
                }),
                None => Schedule {
                    last: None,
                    next_after: s.next_refresh_after,
                },
            };
            Some((s.id.clone(), schedule))
        })
        .collect();
    register();
    let after = ids(rt);
    // A credential registered afresh starts without the schedule of an earlier one
    // under its ID that has not been reconciled away yet.
    schedules.retain(|id, _| after.contains(id) && before.contains(id));
    schedules.extend(entries);
}

/// Records a committed plugin refresh of `c` (Go `refreshAuth` on success): the refresh
/// time, and `NextRefreshAfter` 30 s on when the credential is still due
/// (`refreshIneffectiveBackoff`).
pub(crate) fn committed(schedules: &Schedules, c: &Credential, now: DateTime<Utc>) {
    let still_due = cpa_exec::preferred_interval(c).is_some_and(|interval| {
        cpa_exec::expiration(c).is_some_and(|expiry| expiry.checked_sub_signed(interval).is_none_or(|at| at <= now))
    });
    lock(schedules).insert(
        c.id.clone(),
        Schedule {
            last: Some(now),
            next_after: still_due.then(|| now + chrono::Duration::seconds(30)),
        },
    );
}

/// Records a failed plugin refresh of `c`, whatever triggered it: Go holds the next one
/// off for `refreshFailureBackoff` (5 min), or until the access token expires when
/// that comes sooner.
// ponytail: Go's other failure branches (an invalid grant's growing backoff, a 401 on an
// expired token retrying at once, a disabled credential) all take this 5 min; port them
// if a plugin needs them.
pub(crate) fn failed(rt: &Runtime, c: &Credential, now: DateTime<Utc>) {
    let mut at = now + chrono::Duration::minutes(5);
    if let Some(expiry) = cpa_exec::expiration(c).filter(|expiry| *expiry > now) {
        at = at.min(expiry);
    }
    lock(&rt.plugin_runtime().schedules)
        .entry(c.id.clone())
        .or_default()
        .next_after = Some(at);
}

/// What a plugin refresh commits: the metadata change, and whether a plugin answered.
/// An answer writes Go's merged `proxy_url` into the metadata, and the commit publishes
/// the committed metadata's value as the credential's proxy URL (Go keeps `ProxyURL`
/// and `Metadata["proxy_url"]` equal), so a concurrent edit that wins the metadata wins
/// both.
pub(crate) struct Refreshed {
    pub patch: MetadataPatch,
    pub answered: bool,
}

/// Refreshes `c` through its plugin and returns what to commit. `stored` is the
/// credential `c` was made from for the executor (which may route it through another
/// proxy).
pub(crate) async fn refresh(
    rt: &Runtime,
    route: &Route,
    c: &Credential,
    stored: &Credential,
) -> Result<Refreshed, ExecError> {
    let result = refresh_once(rt, route, c, stored).await;
    if result.is_err() {
        failed(rt, c, Utc::now());
    }
    result
}

// ponytail: the credential's JSON document and its proxy URL are committed (a
// file-backed one; the commit refuses config-backed credentials, as for built-in
// refreshes). Go's document is the refreshed storage overlaid with the merged metadata,
// its `disabled` and canonical `proxy_url` included, and so is this one. A response
// that moves the credential to another provider or ID is refused (Go saves the new
// provider's type, and cannot commit another ID). Go merges changes to the prefix and
// attributes into the running auth only, and keeps its label; here they are not
// applied (a warning names them) until the file is next loaded, which gives both the
// parsed values. Upgrade: a store write that also replaces the prefix and attributes.
async fn refresh_once(
    rt: &Runtime,
    route: &Route,
    c: &Credential,
    stored: &Credential,
) -> Result<Refreshed, ExecError> {
    let metadata: cpa_plugin::gojson::Metadata = c.metadata.clone().into_iter().collect();
    let view = cpa_plugin::auth::AuthView {
        id: c.id.clone(),
        provider: c.provider.clone(),
        file_name: c
            .attributes
            .get("path")
            .and_then(|p| std::path::Path::new(p).file_name())
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default(),
        label: c.label.clone(),
        prefix: c.attributes.get("prefix").cloned().unwrap_or_default(),
        proxy_url: c.attributes.get("proxy_url").cloned().unwrap_or_default(),
        // Go `storageJSONFromAuth`: the credential's JSON document.
        storage_json: bytes::Bytes::from(cpa_plugin::gojson::to_vec(&metadata)),
        metadata,
        attributes: crate::management::quota::go_attributes(c),
        ..Default::default()
    };
    let refreshed = rt
        .plugins()
        .refresh_auth(&view, &Default::default())
        .await
        .map_err(|e| {
            let status = match e.status() {
                0 => 500,
                status => status,
            };
            ExecError::local(status, FailureScope::Credential, e.to_string())
        })?;
    let own_proxy = stored.attributes.get("proxy_url").map_or("", |p| p.trim()).to_owned();
    let Some(mut refreshed) = refreshed else {
        // No plugin answers for the credential's provider.
        let unchanged = Refreshed {
            patch: MetadataPatch::default(),
            answered: false,
        };
        return match route {
            Route::Executor => Ok(unchanged),
            Route::Compat { identifier } => {
                let filled = |k: &str| c.str(k).is_some_and(|v| !v.trim().is_empty());
                if filled("refresh_token") || filled("refreshToken") {
                    Err(ExecError::local(
                        500,
                        FailureScope::Credential,
                        format!("plugin auth provider refresh is unavailable for provider {identifier}"),
                    ))
                } else {
                    Ok(unchanged)
                }
            }
        };
    };
    let (from, to) = (
        cpa_plugin::host::normalize_provider(&c.provider),
        cpa_plugin::host::normalize_provider(&refreshed.provider),
    );
    if to != from || refreshed.id != c.id {
        // Go saves the refreshed storage with the new provider's `type`, so the file
        // moves to that provider at its next load, and fails to commit another ID ("auth
        // not found"). Neither is a refresh this store can commit.
        let change = if to != from {
            format!("provider from {from} to {to}")
        } else {
            format!("ID to {}", refreshed.id)
        };
        return Err(ExecError::local(
            500,
            FailureScope::Credential,
            format!("plugin auth refresh changed the credential's {change}; refusing to save it"),
        ));
    }
    // Go `MergeRefreshedAuth` with nothing edited meanwhile (a concurrent edit is merged
    // at commit): the response's `disabled`, and the three-way proxy URL of the auth's
    // field (`proxy_url` attribute here) and its metadata, written to the metadata.
    let base = (view.proxy_url.trim(), c.str("proxy_url").unwrap_or_default().trim());
    let proxy_meta = refreshed
        .metadata
        .get("proxy_url")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let updated = (refreshed.proxy_url.trim().to_owned(), proxy_meta.trim().to_owned());
    let proxy = match (updated.0 != base.0, updated.1 != base.1) {
        (true, false) => updated.0.as_str(),
        (false, true) => updated.1.as_str(),
        (true, true) if !updated.0.is_empty() => updated.0.as_str(),
        (true, true) => updated.1.as_str(),
        // Unchanged: the stored credential's own proxy URL.
        (false, false) => {
            if own_proxy.is_empty() {
                base.1
            } else {
                own_proxy.as_str()
            }
        }
    }
    .to_owned();
    if proxy.is_empty() {
        refreshed.metadata.remove("proxy_url");
    } else {
        refreshed.metadata.insert("proxy_url".into(), proxy.into());
    }
    refreshed.metadata.insert("disabled".into(), refreshed.disabled.into());
    let mut ignored = Vec::new();
    if refreshed.prefix.trim() != view.prefix.trim() {
        ignored.push("prefix".to_owned());
    }
    if refreshed.label.trim() != view.label.trim() {
        ignored.push("label".to_owned());
    }
    // `path`, `source` and `source_backend` are filled in from the file.
    ignored.extend(
        refreshed
            .attributes
            .iter()
            .filter(|(k, v)| {
                !matches!(k.as_str(), "path" | "source" | "source_backend") && view.attributes.get(*k) != Some(*v)
            })
            .map(|(k, _)| format!("attributes.{k}")),
    );
    if !ignored.is_empty() {
        tracing::warn!(
            credential = %c.id,
            fields = %ignored.join(", "),
            "plugin refresh response changes not applied until the file is next loaded; the refreshed tokens and document are saved"
        );
    }
    // What Go's `pluginTokenStorage` saves: the storage overlaid with the metadata. Go
    // logs a failure to persist storage that does not decode and still publishes the
    // refreshed auth; here the refresh fails (docs/DIFFERENCES-FROM-GO.md).
    let next: Map<String, Value> = refreshed
        .storage_payload()
        .and_then(|payload| serde_json::from_slice(&payload).map_err(|e| e.to_string()))
        .map_err(|e| ExecError::local(500, FailureScope::Credential, format!("persist refreshed auth: {e}")))?;
    Ok(Refreshed {
        patch: diff(&c.metadata, &next),
        answered: true,
    })
}

/// The patch turning `current` into `next`. `type` stays: a refresh that moves the
/// credential to another provider is refused before this.
fn diff(current: &Map<String, Value>, next: &Map<String, Value>) -> MetadataPatch {
    let kept = |k: &str| k != "type";
    MetadataPatch {
        set: next
            .iter()
            .filter(|(k, v)| kept(k) && current.get(*k) != Some(v))
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect(),
        remove: current
            .keys()
            .filter(|k| kept(k) && !next.contains_key(*k))
            .cloned()
            .collect(),
    }
}
