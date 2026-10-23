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
    let schedule = lock(rt.store().refresh_schedules())
        .get(&c.id)
        .copied()
        .unwrap_or_default();
    // Go starts refreshing once its first load, which asks plugins, has registered the
    // credentials; until then their deadlines are not known.
    if schedule.provisional {
        return None;
    }
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

/// Go's refresh lifecycle fields of one credential a plugin refreshes, kept per
/// credential ID beside the store's set ([`register`]).
#[derive(Clone, Copy, Default, Debug, PartialEq)]
pub(crate) struct Schedule {
    /// Go `LastRefreshedAt`.
    pub last: Option<DateTime<Utc>>,
    /// Go `NextRefreshAfter`.
    pub next_after: Option<DateTime<Utc>>,
    /// Registered by the startup load, which runs before plugins parse auth files: never
    /// due ([`due_at`]), and the first plugin-aware load registers the credential as
    /// Go's first one does, taking its own fields ([`end_bootstrap`]).
    pub provisional: bool,
}

/// Credential ID to its [`Schedule`].
pub(crate) type Schedules = Arc<Mutex<HashMap<String, Schedule>>>;

fn lock(schedules: &Schedules) -> std::sync::MutexGuard<'_, HashMap<String, Schedule>> {
    schedules.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Where [`committed`] records plugin refreshes; cloned into the commit, which a
/// cancelled caller cannot stop.
pub(crate) fn schedules(rt: &Runtime) -> Schedules {
    rt.store().refresh_schedules().clone()
}

/// Registers the schedules of a change to the store's set from `before` to `after`
/// (the credentials it replaced, and the set or the credentials replacing them); the
/// store calls it with the change, under its write lock. A credential registered
/// again, under a new revision, keeps its `LastRefreshedAt` and `NextRefreshAfter`
/// while it and the one it replaces are both enabled, and otherwise takes those it is
/// registered with: no `LastRefreshedAt`, and the `NextRefreshAfter` its plugin parsed
/// (Go `prepareCoreAuthForModelRegistration`, service_auth.go L427-450). An auth save
/// (`saved`) registers its credentials with the fields of the auth each replaces,
/// whatever that one's state, unset ones included (Go `preserveExistingAuthState` and
/// `upsertAuthRecordsForPath`), or with their own without one. Go's watcher then
/// registers the written file again (its hash changed; nothing suppresses the server's
/// own writes), against the saved auth: a disabled saved credential ends with its own
/// fields, and that is what it gets here, as the store's reload may find the file's
/// credential unchanged and register nothing. A credential that
/// leaves the set loses its schedule (Go `Manager.Remove`), so the map holds the
/// set's IDs only. A [`Schedule::provisional`] one is replaced as if a side were
/// disabled.
pub(crate) fn register(
    schedules: &Schedules,
    before: &[Arc<Credential>],
    after: &[Arc<Credential>],
    saved: &[SavedRefresh],
) {
    let mut schedules = lock(schedules);
    let previous: HashMap<&str, &Credential> = before.iter().map(|c| (c.id.as_str(), &**c)).collect();
    let mut registered = Vec::new();
    for c in after {
        let previous = previous.get(c.id.as_str());
        if previous.is_some_and(|p| p.revision == c.revision) {
            continue;
        }
        let own = Schedule {
            last: None,
            next_after: c.next_refresh_after.map(DateTime::<Utc>::from),
            provisional: false,
        };
        let schedule = match saved.iter().find(|s| s.id == c.id) {
            // Go's watcher registers the written file again right after the save, which
            // keeps the save's fields only while the credential is enabled.
            Some(SavedRefresh { donor: Some(donor), .. }) if !c.disabled => Schedule {
                provisional: false,
                ..schedules.get(donor).copied().unwrap_or_default()
            },
            Some(_) => own,
            None if previous.is_some_and(|p| !p.disabled && !c.disabled)
                && !schedules.get(&c.id).is_some_and(|s| s.provisional) =>
            {
                continue;
            }
            None => own,
        };
        registered.push((c.id.clone(), schedule));
    }
    let live: std::collections::HashSet<&str> = after.iter().map(|c| c.id.as_str()).collect();
    for id in previous.keys().filter(|id| !live.contains(*id)) {
        schedules.remove(*id);
    }
    schedules.extend(registered);
}

/// Marks every schedule the startup load registered [`Schedule::provisional`].
pub(crate) fn begin_bootstrap(schedules: &Schedules) {
    for schedule in lock(schedules).values_mut() {
        schedule.provisional = true;
    }
}

/// Ends the startup window: the first plugin-aware load has registered its credentials,
/// and the schedules it left in place are their own. Whether any was provisional.
pub(crate) fn end_bootstrap(schedules: &Schedules) -> bool {
    let mut ended = false;
    for schedule in lock(schedules).values_mut() {
        ended |= std::mem::take(&mut schedule.provisional);
    }
    ended
}

/// The IDs whose schedules are [`Schedule::provisional`].
pub(crate) fn provisional(schedules: &Schedules) -> std::collections::HashSet<String> {
    lock(schedules)
        .iter()
        .filter(|(_, s)| s.provisional)
        .map(|(id, _)| id.clone())
        .collect()
}

/// Records a committed plugin refresh of `c` (Go `refreshAuth` on success): the refresh
/// time, and `NextRefreshAfter` 30 s on when the credential is still due
/// (`refreshIneffectiveBackoff`).
pub(crate) fn committed(schedules: &Schedules, c: &Credential, now: DateTime<Utc>) {
    let still_due = cpa_exec::preferred_interval(c).is_some_and(|interval| {
        cpa_exec::expiration(c).is_some_and(|expiry| expiry.checked_sub_signed(interval).is_none_or(|at| at <= now))
    });
    // Only for a credential still in the set: a removed one has no schedule to record
    // into, and a late result must not leave one behind.
    if let Some(schedule) = lock(schedules).get_mut(&c.id) {
        schedule.last = Some(now);
        schedule.next_after = still_due.then(|| now + chrono::Duration::seconds(30));
    }
}

/// Records a failed plugin refresh of `c`, whatever triggered it: Go holds the next one
/// off for `refreshFailureBackoff` (5 min), or until the access token expires when
/// that comes sooner.
// ponytail: Go's other failure branches (an invalid grant's growing backoff, a 401 on an
// expired token retrying at once, a disabled credential) all take this 5 min; port them
// if a plugin needs them.
pub(crate) fn failed(store: &crate::runtime::CredentialStore, c: &Credential, now: DateTime<Utc>) {
    let mut at = now + chrono::Duration::minutes(5);
    if let Some(expiry) = cpa_exec::expiration(c).filter(|expiry| *expiry > now) {
        at = at.min(expiry);
    }
    // As in [`committed`], only for a credential still in the set.
    if let Some(schedule) = lock(store.refresh_schedules()).get_mut(&c.id) {
        schedule.next_after = Some(at);
    }
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
        failed(rt.store(), c, Utc::now());
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

#[cfg(test)]
mod tests {
    use super::*;

    /// A refresh result that lands after its credential left the set records nothing:
    /// no schedule outlives its credential, whatever the churn.
    #[test]
    fn a_late_refresh_result_leaves_no_schedule_behind() {
        let credential = |id: &str| {
            let metadata = serde_json::json!({"type":"pp","access_token":"t","refresh_interval":"1h"});
            Credential::from_file(
                std::path::Path::new("/auths"),
                &std::path::Path::new("/auths").join(id),
                metadata.as_object().cloned().unwrap(),
            )
            .unwrap()
        };
        let store = crate::runtime::CredentialStore::new(vec![credential("a.json")]);
        store.end_bootstrap();
        let now = Utc::now();
        for round in 0..3 {
            let gone = store.get("a.json").unwrap();
            store.reconcile(vec![credential(&format!("b{round}.json"))]);
            committed(store.refresh_schedules(), &gone, now);
            failed(&store, &gone, now);
            let ids: Vec<String> = lock(store.refresh_schedules()).keys().cloned().collect();
            assert_eq!(ids, [format!("b{round}.json")], "round {round}");
            store.reconcile(vec![credential("a.json")]);
        }
    }
}
