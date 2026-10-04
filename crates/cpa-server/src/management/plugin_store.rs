//! Plugin store routes (internal/api/handlers/management/plugin_store.go and
//! plugin_store_release.go): the catalog of every configured registry with the local
//! install state, and installs that write the library and enable it in config.

use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, SystemTime};

use axum::body::Bytes;
use axum::extract::{Path as UrlPath, RawQuery, State};
use axum::http::{HeaderValue, StatusCode};
use axum::response::Response;
use cpa_exec::proxy::Proxy;
use cpa_plugin::config as pcfg;
use cpa_plugin::store::{
    self, AuthConfig, Client, Doer, GitHubRateLimiter, INSTALL_TYPE_DIRECT, INSTALL_TYPE_GITHUB_RELEASE,
    InstallOptions, InstallResult, Manifest, Plugin, Release, Source, StoreError,
};
use serde_json::{Value, json};
use serde_yaml_ng::Value as Yaml;

use super::Management;
use super::plugins::{
    Fail, PluginsView, Res, current_node, discover, error, esc, go_query, map_json, plugin_id, resolved_dir, save,
    set_key, set_node, struct_json,
};

const RELEASE_TTL: Duration = Duration::from_secs(3600);
const RELEASE_FAILURE_TTL: Duration = Duration::from_secs(30);
const RELEASE_CONCURRENCY: usize = 2;

/// The store's per-server state: Go's `Handler.pluginStore*` fields and release cache.
pub(crate) struct StoreState {
    /// Go `pluginStoreRegistryURL` (tests): when set, the only source.
    registry_url: String,
    /// Go `pluginStoreHTTPClient` (tests); production uses Go's client for the proxy.
    http: Option<Arc<dyn Doer>>,
    /// Go `pluginStoreRateLimiter`; `None` shares the process-wide limiter.
    limiter: Option<Arc<GitHubRateLimiter>>,
    releases: ReleaseCache,
}

impl StoreState {
    pub(crate) fn new(
        registry_url: Option<String>,
        http: Option<Arc<dyn Doer>>,
        limiter: Option<Arc<GitHubRateLimiter>>,
    ) -> Self {
        Self {
            registry_url: registry_url.unwrap_or_default(),
            http,
            limiter,
            releases: ReleaseCache::default(),
        }
    }
}

// ---- snapshot, sources, clients ------------------------------------------------------

/// Go `pluginStoreSnapshot`.
struct Snapshot {
    view: PluginsView,
    proxy_url: String,
    sources: Vec<String>,
    auth: Vec<AuthConfig>,
}

fn snapshot(state: &Management) -> Snapshot {
    let cfg = state.rt.config();
    let proxy_url = cfg
        .document
        .get("requests")
        .and_then(|r| r.get("proxy-url"))
        .and_then(Yaml::as_str)
        .unwrap_or_default()
        .trim()
        .to_owned();
    Snapshot {
        view: PluginsView::of(&cfg),
        proxy_url,
        sources: pcfg::store_sources(&cfg.document),
        auth: pcfg::store_auth(&cfg.document),
    }
}

/// Go `pluginStoreSources`.
fn sources(state: &Management, configured: &[String]) -> Res<Vec<Source>> {
    let registry_url = state.plugin_store.registry_url.trim();
    if !registry_url.is_empty() {
        let mut source = store::default_source();
        source.url = registry_url.to_owned();
        return Ok(vec![source]);
    }
    store::normalize_sources(configured).map_err(|e| {
        Box::new(error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "plugin_store_source_invalid",
            &e,
        ))
    })
}

/// Go `newPluginStoreClient`.
fn new_client(state: &Management, proxy_url: &str, registry_url: &str, auth: &[AuthConfig]) -> Client {
    let http: Arc<dyn Doer> = match &state.plugin_store.http {
        Some(http) => http.clone(),
        None => Arc::new(store::WreqDoer::new(state.clients.get(&Proxy::parse(proxy_url)))),
    };
    let mut client = Client::new(http);
    client.network_scope = proxy_url.trim().to_owned();
    client.rate_limiter = state.plugin_store.limiter.clone();
    client.registry_url = match registry_url.trim() {
        "" => store::DEFAULT_REGISTRY_URL.to_owned(),
        url => url.to_owned(),
    };
    client.auth = auth.to_vec();
    client
}

struct SourceError {
    source: Source,
    cause: StoreError,
}

/// Go `fetchSourcedPlugins`: every source's registry, in order.
async fn fetch_sourced_plugins(
    state: &Management,
    snap: &Snapshot,
    sources: &[Source],
) -> (Vec<(Source, Plugin)>, Vec<SourceError>) {
    let mut plugins = Vec::new();
    let mut errors = Vec::new();
    for source in sources {
        let client = new_client(state, &snap.proxy_url, &source.url, &snap.auth);
        match client.fetch_registry().await {
            Ok(registry) => plugins.extend(registry.plugins.into_iter().map(|p| (source.clone(), p))),
            Err(cause) => errors.push(SourceError {
                source: source.clone(),
                cause,
            }),
        }
    }
    (plugins, errors)
}

fn sources_json(sources: impl IntoIterator<Item = Source>) -> Value {
    Value::Array(
        sources
            .into_iter()
            .map(|s| json!({"id": esc(&s.id), "name": esc(&s.name), "url": esc(&s.url)}))
            .collect(),
    )
}

/// Go `writePluginStoreRateLimit`.
fn rate_limited(err: &StoreError) -> Option<Response> {
    let rate = err.rate_limit()?;
    let retry_after = rate.retry_after_seconds(SystemTime::now());
    let retry_at = chrono::DateTime::<chrono::Utc>::from(rate.retry_at)
        .format("%Y-%m-%dT%H:%M:%SZ")
        .to_string();
    let mut response = map_json(
        StatusCode::TOO_MANY_REQUESTS,
        &json!({
            "error": "plugin_store_rate_limited",
            "message": rate.to_string(),
            "retry_after": retry_after,
            "retry_at": retry_at,
        }),
    );
    response.headers_mut().insert(
        "Retry-After",
        HeaderValue::from_str(&retry_after.to_string()).expect("digits"),
    );
    Some(response)
}

// ---- local install state ---------------------------------------------------------------

/// Go `pluginLocalStatus`.
#[derive(Default, Clone)]
struct LocalStatus {
    installed: bool,
    installed_version: String,
    store_managed: bool,
    installed_source_id: String,
    installed_source_url: String,
    path: String,
    configured: bool,
    registered: bool,
    enabled: bool,
    effective_enabled: bool,
}

/// Go `pluginStoreConfiguredSource`: the source a store-managed config names (a
/// `store` node that fails Go's typed decode is managed with no known source).
fn configured_source(raw: &Yaml) -> (String, String, bool) {
    let Some(store) = raw.as_mapping().and_then(|m| m.get("store")) else {
        return (String::new(), String::new(), false);
    };
    match Manifest::from_yaml(store) {
        Some(m) => (m.source_id.trim().to_owned(), m.source_url.trim().to_owned(), true),
        None => (String::new(), String::new(), true),
    }
}

/// Go `pluginLocalStatuses`.
fn local_statuses(state: &Management, view: &PluginsView, dir: &Path) -> Res<HashMap<String, LocalStatus>> {
    let mut statuses: HashMap<String, LocalStatus> = HashMap::new();
    for file in discover(dir, &view.desired_versions())? {
        let status = statuses.entry(file.id.clone()).or_default();
        status.installed = true;
        status.path = file.path.to_string_lossy().into_owned();
        if !file.version.trim().is_empty() {
            status.installed_version = file.version.trim().to_owned();
        }
        status.enabled = true;
    }
    for (id, (item, raw)) in &view.configs {
        let status = statuses.entry(id.clone()).or_default();
        status.configured = true;
        status.enabled = item.enabled;
        (
            status.installed_source_id,
            status.installed_source_url,
            status.store_managed,
        ) = configured_source(raw);
    }
    for info in state.rt.plugins().registered_plugins() {
        let status = statuses.entry(info.id.clone()).or_default();
        status.installed = true;
        status.registered = true;
        status.installed_version = info.metadata.version.trim().to_owned();
    }
    for status in statuses.values_mut() {
        status.effective_enabled = view.enabled && status.enabled && status.registered;
    }
    Ok(statuses)
}

/// Go `pluginStoreResolveInstalledSource`.
fn resolve_installed_source(status: &LocalStatus, sources: &[Source]) -> Option<String> {
    let id = status.installed_source_id.trim();
    let url = status.installed_source_url.trim();
    if !id.is_empty() {
        return match sources.iter().find(|s| s.id.trim() == id) {
            Some(source) if !url.is_empty() && source.url.trim() != url => None,
            _ => Some(id.to_owned()),
        };
    }
    if url.is_empty() {
        return None;
    }
    sources
        .iter()
        .find(|s| s.url.trim() == url)
        .map(|s| s.id.trim().to_owned())
}

/// Go `pluginStoreInstallSourceStatus`: (installed source, status, whether this entry
/// may update the installed plugin).
fn install_source_status(
    status: &LocalStatus,
    sources: &[Source],
    entry_source_id: &str,
    source_count: usize,
) -> (String, &'static str, bool) {
    if !status.installed && !status.configured && !status.registered {
        return (String::new(), "", true);
    }
    if let Some(id) = resolve_installed_source(status, sources) {
        let matched = id == entry_source_id.trim();
        return (id, if matched { "matched" } else { "different" }, matched);
    }
    if status.store_managed || source_count > 1 {
        return (String::new(), "unknown", false);
    }
    (String::new(), "assumed", true)
}

// ---- release cache (plugin_store_release.go) -----------------------------------------------

#[derive(Default, Clone)]
struct CacheEntry {
    version: String,
    next_check_at: Option<SystemTime>,
}

/// A lookup's outcome for its waiters: the version, and whether the leader was
/// cancelled before finishing.
type Outcome = Option<(String, bool)>;

#[derive(Default)]
struct CacheState {
    entries: HashMap<String, CacheEntry>,
    inflight: HashMap<String, tokio::sync::watch::Receiver<Outcome>>,
}

/// Go `pluginReleaseCache`: in-flight lookups and the concurrency budget are shared by
/// every catalog request on this server.
pub(crate) struct ReleaseCache {
    state: Mutex<CacheState>,
    slots: tokio::sync::Semaphore,
}

impl Default for ReleaseCache {
    fn default() -> Self {
        Self {
            state: Mutex::default(),
            slots: tokio::sync::Semaphore::new(RELEASE_CONCURRENCY),
        }
    }
}

/// Go `pluginReleaseKey`.
fn release_key(client: &Client, plugin: &Plugin) -> String {
    if store::plugin_install_type(plugin) != INSTALL_TYPE_GITHUB_RELEASE || plugin.repository.is_empty() {
        return String::new();
    }
    client.latest_release_cache_key(plugin).unwrap_or_default()
}

/// The leader's claim on a key; dropping it unfinished (the request went away) stores
/// the entry as it was and wakes the waiters, one of which takes over.
struct Leader<'a> {
    cache: &'a ReleaseCache,
    key: String,
    entry: CacheEntry,
    done: tokio::sync::watch::Sender<Outcome>,
    finished: bool,
}

impl Leader<'_> {
    fn finish(mut self, entry: CacheEntry) -> String {
        self.finished = true;
        self.publish(entry.clone(), false);
        entry.version
    }

    fn publish(&self, entry: CacheEntry, canceled: bool) {
        let mut state = self.cache.state.lock().unwrap_or_else(PoisonError::into_inner);
        let version = entry.version.clone();
        state.entries.insert(self.key.clone(), entry);
        state.inflight.remove(&self.key);
        self.done.send_replace(Some((version, canceled)));
    }
}

impl Drop for Leader<'_> {
    fn drop(&mut self) {
        if !self.finished {
            self.publish(self.entry.clone(), true);
        }
    }
}

impl ReleaseCache {
    /// Go `cached`: the last result, without network activity.
    fn cached(&self, client: &Client, plugin: &Plugin) -> String {
        let key = release_key(client, plugin);
        let state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        state.entries.get(&key).map(|e| e.version.clone()).unwrap_or_default()
    }

    /// Go `latestPluginVersion`.
    async fn latest(&self, client: &Client, plugin: &Plugin) -> String {
        if store::plugin_install_type(plugin) != INSTALL_TYPE_GITHUB_RELEASE {
            return String::new();
        }
        let Ok((client, key)) = client.prepare_latest_release(plugin) else {
            return String::new();
        };
        let leader = loop {
            let mut pending = {
                let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
                let entry = state.entries.get(&key).cloned().unwrap_or_default();
                if entry.next_check_at.is_some_and(|at| SystemTime::now() < at) {
                    return entry.version;
                }
                match state.inflight.get(&key) {
                    Some(pending) => pending.clone(),
                    None => {
                        let (done, pending) = tokio::sync::watch::channel(None);
                        state.inflight.insert(key.clone(), pending);
                        break Leader {
                            cache: self,
                            key: key.clone(),
                            entry,
                            done,
                            finished: false,
                        };
                    }
                }
            };
            // A live waiter takes over after a cancelled leader; the others coalesce
            // behind the replacement lookup.
            match pending.wait_for(Option::is_some).await.map(|o| o.clone()) {
                Ok(Some((version, false))) => return version,
                _ => continue,
            }
        };
        // Every listing shares the slots; a cancelled waiter never takes one.
        let _slot = self.slots.acquire().await.expect("never closed");
        let result = match client.fetch_latest_release(plugin).await {
            Ok(release) => store::release_version(&release).map_err(StoreError::Other),
            Err(e) => Err(e),
        };
        let mut entry = leader.entry.clone();
        let mut ttl = RELEASE_FAILURE_TTL;
        match &result {
            Ok(version) => {
                entry.version = version.clone();
                ttl = RELEASE_TTL;
            }
            Err(e) => {
                tracing::warn!(error = %e, plugin_id = %plugin.id, "pluginstore: failed to fetch latest release");
            }
        }
        // The TTL starts when the lookup finishes, not when it was queued.
        entry.next_check_at = Some(SystemTime::now() + ttl);
        if let Err(StoreError::RateLimited(rate)) = &result {
            entry.next_check_at = Some(rate.retry_at);
        }
        leader.finish(entry)
    }

    /// Go `latestPluginVersions`: catalog order, skipped placeholders included.
    async fn latest_all(&self, client: &Client, plugins: &[Option<Plugin>]) -> Vec<String> {
        futures_util::future::join_all(plugins.iter().map(|plugin| async move {
            match plugin {
                Some(plugin) if !release_key(client, plugin).is_empty() => self.latest(client, plugin).await,
                _ => String::new(),
            }
        }))
        .await
    }
}

// ---- GET /plugins/store ------------------------------------------------------------------

/// Go `ListPluginStore`.
pub(crate) async fn list(State(state): State<Arc<Management>>) -> Response {
    match list_inner(&state).await {
        Ok(r) => r,
        Err(r) => *r,
    }
}

async fn list_inner(state: &Management) -> Res<Response> {
    let snap = snapshot(state);
    let dir = resolved_dir(&snap.view.dir)?;
    let sources = sources(state, &snap.sources)?;
    let (plugins, source_errors) = fetch_sourced_plugins(state, &snap, &sources).await;
    if plugins.is_empty()
        && let Some(first) = source_errors.first()
    {
        return Err(Box::new(error(
            StatusCode::BAD_GATEWAY,
            "plugin_store_registry_failed",
            &first.cause.to_string(),
        )));
    }
    let statuses = local_statuses(state, &snap.view, &dir)?;
    let mut counts: HashMap<&str, usize> = HashMap::new();
    for (_, plugin) in &plugins {
        *counts.entry(plugin.id.as_str()).or_default() += 1;
    }
    let status_of = |id: &str| statuses.get(id).cloned().unwrap_or_default();
    // Browsing must not spend API quota on uninstalled plugins or on sources that cannot
    // update the installed one; placeholders keep versions aligned with the catalog.
    let latest_input: Vec<Option<Plugin>> = plugins
        .iter()
        .map(|(source, plugin)| {
            let status = status_of(&plugin.id);
            let (_, _, allows_update) =
                install_source_status(&status, &sources, &source.id, counts[plugin.id.as_str()]);
            (status.installed && allows_update).then(|| plugin.clone())
        })
        .collect();
    let client = new_client(state, &snap.proxy_url, "", &snap.auth);
    let latest = state.plugin_store.releases.latest_all(&client, &latest_input).await;

    let mut entries = Vec::with_capacity(plugins.len());
    for (index, (source, plugin)) in plugins.iter().enumerate() {
        let status = status_of(&plugin.id);
        let (installed_source_id, source_status, allows_update) =
            install_source_status(&status, &sources, &source.id, counts[plugin.id.as_str()]);
        // The registry version when the latest release is unknown.
        let store_version = if !latest[index].is_empty() {
            latest[index].clone()
        } else {
            match state.plugin_store.releases.cached(&client, plugin) {
                cached if !cached.is_empty() => cached,
                _ => plugin.version.clone(),
            }
        };
        let mut entry = json!({
            "store_id": esc(&format!("{}/{}", source.id, plugin.id)),
            "source_id": esc(&source.id),
            "source_name": esc(&source.name),
            "source_url": esc(&source.url),
            "id": esc(&plugin.id),
            "name": esc(&plugin.name),
            "description": esc(&plugin.description),
            "author": esc(&plugin.author),
            "version": esc(&store_version),
            "repository": esc(&plugin.repository),
            "install_type": esc(&store::plugin_install_type(plugin)),
            "auth_required": plugin.auth_required,
            "auth_configured": store::plugin_auth_configured(source, plugin, &snap.auth),
        });
        let map = entry.as_object_mut().expect("object");
        let platforms = store::plugin_platforms(plugin);
        if !platforms.is_empty() {
            map.insert(
                "platforms".into(),
                platforms
                    .iter()
                    .map(|p| json!({"goos": esc(&p.goos), "goarch": esc(&p.goarch)}))
                    .collect(),
            );
        }
        for (key, value) in [
            ("logo", &plugin.logo),
            ("homepage", &plugin.homepage),
            ("license", &plugin.license),
        ] {
            if !value.is_empty() {
                map.insert(key.into(), esc(value).into());
            }
        }
        if !plugin.tags.is_empty() {
            map.insert("tags".into(), plugin.tags.iter().map(|t| esc(t)).collect());
        }
        map.insert("installed".into(), status.installed.into());
        map.insert("installed_version".into(), esc(&status.installed_version).into());
        if !installed_source_id.is_empty() {
            map.insert("installed_source_id".into(), esc(&installed_source_id).into());
        }
        if !source_status.is_empty() {
            map.insert("install_source_status".into(), source_status.into());
        }
        map.insert("path".into(), esc(&status.path).into());
        map.insert("configured".into(), status.configured.into());
        map.insert("registered".into(), status.registered.into());
        map.insert("enabled".into(), status.enabled.into());
        map.insert("effective_enabled".into(), status.effective_enabled.into());
        map.insert(
            "update_available".into(),
            (allows_update && store::update_available(&status.installed_version, &store_version)).into(),
        );
        entries.push(entry);
    }

    let mut body = json!({
        "plugins_enabled": snap.view.enabled,
        "plugins_dir": esc(&dir.to_string_lossy()),
        "sources": sources_json(sources.iter().cloned()),
    });
    let map = body.as_object_mut().expect("object");
    if !source_errors.is_empty() {
        map.insert(
            "source_errors".into(),
            source_errors
                .iter()
                .map(|e| {
                    json!({
                        "source_id": esc(&e.source.id),
                        "source_name": esc(&e.source.name),
                        "source_url": esc(&e.source.url),
                        "message": esc(&e.cause.to_string()),
                    })
                })
                .collect(),
        );
    }
    map.insert("plugins".into(), Value::Array(entries));
    Ok(struct_json(StatusCode::OK, &body))
}

// ---- POST /plugins/store/:id/install ------------------------------------------------------

cpa_plugin::go_struct! {
    pub struct InstallRequest("management.pluginInstallRequest") {
        "version" => version: String,
    }
}

fn normalize_requested_version(version: &str) -> String {
    let version = version.trim();
    match version.get(..1) {
        Some(v) if v.eq_ignore_ascii_case("v") => version[1..].trim().to_owned(),
        _ => version.to_owned(),
    }
}

/// Go `pluginStoreReleaseTagCandidates`.
fn release_tag_candidates(version: &str) -> Vec<String> {
    let version = version.trim();
    if version.is_empty() {
        return Vec::new();
    }
    if version.get(..1).is_some_and(|v| v.eq_ignore_ascii_case("v")) {
        return vec![version.to_owned(), version[1..].trim().to_owned()];
    }
    vec![version.to_owned(), format!("v{version}")]
}

/// Go `pluginInstallRequestedVersion`.
fn requested_version(query: &[(String, String)], body: &[u8]) -> Result<String, String> {
    let from_query = query
        .iter()
        .find(|(k, _)| k == "version")
        .map(|(_, v)| v.trim().to_owned())
        .unwrap_or_default();
    if String::from_utf8_lossy(body).trim().is_empty() {
        return Ok(from_query);
    }
    let request: InstallRequest =
        cpa_plugin::gojson::from_slice(body).map_err(|e| format!("decode install request: {e}"))?;
    let from_body = request.version.trim().to_owned();
    if from_query.is_empty() {
        return Ok(from_body);
    }
    if from_body.is_empty() || normalize_requested_version(&from_body) == normalize_requested_version(&from_query) {
        return Ok(from_query);
    }
    Err(format!(
        "version query {} does not match request body version {}",
        cpa_common::gostr::quote(&from_query),
        cpa_common::gostr::quote(&from_body)
    ))
}

/// Go `findPluginStoreInstallTarget`.
async fn install_target(
    state: &Management,
    snap: &Snapshot,
    sources: &[Source],
    id: &str,
    requested_source: &str,
) -> Res<(Source, Plugin, Client)> {
    let requested_source = requested_source.trim();
    if !requested_source.is_empty() {
        let Some(source) = sources.iter().find(|s| s.id == requested_source) else {
            return Err(Box::new(error(
                StatusCode::NOT_FOUND,
                "plugin_store_source_not_found",
                "plugin store source not found",
            )));
        };
        let client = new_client(state, &snap.proxy_url, &source.url, &snap.auth);
        let registry = match client.fetch_registry().await {
            Ok(r) => r,
            Err(e) => {
                return Err(Box::new(rate_limited(&e).unwrap_or_else(|| {
                    error(StatusCode::BAD_GATEWAY, "plugin_store_registry_failed", &e.to_string())
                })));
            }
        };
        let Some(plugin) = registry.plugin_by_id(id).cloned() else {
            return Err(Box::new(error(
                StatusCode::NOT_FOUND,
                "plugin_not_found",
                "plugin not found in registry source",
            )));
        };
        return Ok((source.clone(), plugin, client));
    }
    let (plugins, source_errors) = fetch_sourced_plugins(state, snap, sources).await;
    let mut matches: Vec<(Source, Plugin)> = plugins.iter().filter(|(_, p)| p.id == id).cloned().collect();
    if matches.is_empty() {
        if plugins.is_empty()
            && let Some(first) = source_errors.first()
        {
            return Err(Box::new(rate_limited(&first.cause).unwrap_or_else(|| {
                error(
                    StatusCode::BAD_GATEWAY,
                    "plugin_store_registry_failed",
                    &first.cause.to_string(),
                )
            })));
        }
        return Err(Box::new(error(
            StatusCode::NOT_FOUND,
            "plugin_not_found",
            "plugin not found in registry",
        )));
    }
    if matches.len() > 1 {
        return Err(Box::new(map_json(
            StatusCode::CONFLICT,
            &json!({
                "error": "plugin_store_source_required",
                "message": "multiple plugin store sources contain this plugin id; specify source",
                "sources": sources_json(matches.into_iter().map(|(s, _)| s)),
            }),
        )));
    }
    let (source, plugin) = matches.remove(0);
    let client = new_client(state, &snap.proxy_url, &source.url, &snap.auth);
    Ok((source, plugin, client))
}

/// Go `validatePluginStoreInstallSource`: a store-managed install only updates from its
/// own source.
fn validate_install_source(view: &PluginsView, sources: &[Source], id: &str, requested_source: &str) -> Res<()> {
    let Some((_, raw)) = view.configs.get(id) else {
        return Ok(());
    };
    let (installed_source_id, installed_source_url, managed) = configured_source(raw);
    if !managed {
        return Ok(());
    }
    let status = LocalStatus {
        store_managed: true,
        installed_source_id,
        installed_source_url,
        ..Default::default()
    };
    let requested = requested_source.trim();
    match resolve_installed_source(&status, sources) {
        None => Err(Box::new(map_json(
            StatusCode::CONFLICT,
            &json!({
                "error": "plugin_store_installed_source_unknown",
                "message": "installed plugin source cannot be verified; uninstall it before reinstalling from the store",
                "requested_source_id": requested,
            }),
        ))),
        Some(resolved) if resolved != requested => Err(Box::new(map_json(
            StatusCode::CONFLICT,
            &json!({
                "error": "plugin_store_source_conflict",
                "message": "installed plugin belongs to a different store source; uninstall it before switching sources",
                "installed_source_id": resolved,
                "requested_source_id": requested,
            }),
        ))),
        Some(_) => Ok(()),
    }
}

/// Go `pluginStoreDirectManifest`: the registry's version, or a listed older one.
fn direct_manifest(source: &Source, plugin: &Plugin, requested: &str) -> Result<Manifest, String> {
    let mut version = normalize_requested_version(requested);
    if version.is_empty() {
        version = normalize_requested_version(&plugin.version);
    }
    let mut plugin = plugin.clone();
    if normalize_requested_version(&plugin.version) == version {
        plugin.version = version;
        return store::manifest_from_plugin(source, &plugin);
    }
    let Some(candidate) = plugin
        .versions
        .iter()
        .find(|c| normalize_requested_version(&c.version) == version)
        .cloned()
    else {
        return Err(format!(
            "direct plugin version {} not found",
            cpa_common::gostr::quote(&version)
        ));
    };
    plugin.version = version;
    plugin.install = candidate.install;
    if plugin.install.install_type.trim().is_empty() {
        plugin.install.install_type = INSTALL_TYPE_DIRECT.into();
    }
    store::manifest_from_plugin(source, &plugin)
}

/// Go `installPluginStoreGitHubRelease`: the latest release, or the requested version
/// under its tag spellings.
async fn install_github_release(
    client: &Client,
    plugin: &Plugin,
    requested: &str,
    options: impl Fn() -> InstallOptions,
) -> Result<InstallResult, StoreError> {
    let version = normalize_requested_version(requested);
    if version.is_empty() {
        return client.install(plugin.clone(), options()).await;
    }
    let mut errors = Vec::new();
    let mut locked = false;
    for tag in release_tag_candidates(requested) {
        match client.install_version(plugin.clone(), &tag, &version, options()).await {
            Ok(result) => return Ok(result),
            Err(e @ StoreError::RateLimited(_)) => return Err(e),
            Err(e) => {
                // errors.Is still finds ErrLoadedPluginLocked inside the joined error.
                locked |= matches!(e, StoreError::LoadedPluginLocked);
                errors.push(format!("{tag}: {e}"));
            }
        }
    }
    if locked {
        return Err(StoreError::LoadedPluginLocked);
    }
    Err(StoreError::Other(format!(
        "install release by tag: {}",
        errors.join("\n")
    )))
}

/// Go `pluginStoreManifestForInstall`.
fn manifest_for_install(source: &Source, plugin: &Plugin, result: &InstallResult) -> Result<Manifest, String> {
    let install_type = match result.install_type.trim() {
        "" => store::plugin_install_type(plugin),
        t => t.to_owned(),
    };
    let mut plugin = plugin.clone();
    match install_type.as_str() {
        INSTALL_TYPE_DIRECT => {
            plugin.version = result.version.trim().to_owned();
            plugin.install = store::normalize_install_plan(&plugin.install);
            store::manifest_from_plugin(source, &plugin)
        }
        INSTALL_TYPE_GITHUB_RELEASE => {
            let tag = result.release_tag.trim();
            if tag.is_empty() {
                return Err("release tag is required".into());
            }
            let release = Release {
                tag_name: tag.to_owned(),
                ..Default::default()
            };
            store::manifest_from_release(source, &plugin, &release)
        }
        _ => Err(format!(
            "unsupported install type {}",
            cpa_common::gostr::quote(&result.install_type)
        )),
    }
}

/// Go `InstallPluginFromStore`.
pub(crate) async fn install(
    State(state): State<Arc<Management>>,
    UrlPath(raw): UrlPath<String>,
    RawQuery(query): RawQuery,
    body: Bytes,
) -> Response {
    match install_inner(state, &raw, &go_query(query.as_deref().unwrap_or_default()), &body).await {
        Ok(r) => r,
        Err(r) => *r,
    }
}

async fn install_inner(state: Arc<Management>, raw: &str, query: &[(String, String)], body: &[u8]) -> Res<Response> {
    let id = plugin_id(raw)?;
    let requested =
        requested_version(query, body).map_err(|e| error(StatusCode::BAD_REQUEST, "invalid_request", &e))?;
    let snap = snapshot(&state);
    let dir = resolved_dir(&snap.view.dir)?;
    let sources = sources(&state, &snap.sources)?;
    let requested_source = query
        .iter()
        .find(|(k, _)| k == "source")
        .map(|(_, v)| v.as_str())
        .unwrap_or_default();
    let (source, plugin, client) = install_target(&state, &snap, &sources, &id, requested_source).await?;
    validate_install_source(&snap.view, &sources, &id, &source.id)?;
    let options = || {
        let state = state.clone();
        let id = id.clone();
        InstallOptions {
            plugins_dir: dir.to_string_lossy().into_owned(),
            goos: cpa_plugin::platform::goos().to_owned(),
            goarch: cpa_plugin::platform::goarch().to_owned(),
            plugin_loaded: Some(Box::new(move || state.rt.plugins().plugin_busy(&id))),
            before_write: None,
        }
    };
    let mut manifest = None;
    let result = match store::plugin_install_type(&plugin).as_str() {
        INSTALL_TYPE_DIRECT => {
            let m = direct_manifest(&source, &plugin, &requested)
                .map_err(|e| error(StatusCode::BAD_GATEWAY, "plugin_manifest_invalid", &e))?;
            let result = client.install_manifest(&m, options()).await;
            manifest = Some(m);
            result
        }
        INSTALL_TYPE_GITHUB_RELEASE => install_github_release(&client, &plugin, &requested, options).await,
        _ => {
            return Err(Box::new(error(
                StatusCode::BAD_GATEWAY,
                "plugin_manifest_invalid",
                &format!(
                    "unsupported install type {}",
                    cpa_common::gostr::quote(&plugin.install.install_type)
                ),
            )));
        }
    };
    let result = match result {
        Ok(r) => r,
        Err(e) => {
            if let Some(r) = rate_limited(&e) {
                return Err(Box::new(r));
            }
            if matches!(e, StoreError::LoadedPluginLocked) {
                return Err(Box::new(map_json(
                    StatusCode::CONFLICT,
                    &json!({
                        "error": "plugin_update_requires_restart",
                        "message": "loaded plugin cannot be overwritten while the server is running",
                        "restart_required": true,
                    }),
                )));
            }
            return Err(Box::new(error(
                StatusCode::BAD_GATEWAY,
                "plugin_install_failed",
                &e.to_string(),
            )));
        }
    };
    let manifest = match manifest {
        Some(m) => m,
        None => manifest_for_install(&source, &plugin, &result).map_err(|e| {
            map_json(
                StatusCode::INTERNAL_SERVER_ERROR,
                &json!({
                    "error": "plugin_manifest_failed",
                    "message": format!("plugin file installed at {} but creating store manifest failed: {e}", result.path),
                    "path": result.path,
                }),
            )
        })?,
    };

    // Go `enablePluginConfigLocked`, then the save.
    let store_node = manifest.to_yaml();
    let id_for_edit = id.clone();
    let saved = tokio::task::spawn_blocking({
        let state = state.clone();
        move || {
            save(&state, move |doc| {
                let mut node = current_node(doc, &id_for_edit);
                set_key(&mut node, "enabled", true.into());
                set_key(&mut node, "store", store_node);
                set_node(doc, &id_for_edit, node)
            })
        }
    })
    .await;
    let failed = |code: &str, what: &str, e: &str| {
        map_json(
            StatusCode::INTERNAL_SERVER_ERROR,
            &json!({
                "error": code,
                "message": format!("plugin file installed at {} but {what} failed: {e}", result.path),
                "path": result.path,
            }),
        )
    };
    match saved {
        Ok(Ok(())) => {}
        Ok(Err(Fail::Save(e))) => return Err(Box::new(failed("config_save_failed", "saving config", &e))),
        Ok(Err(Fail::Response(r))) => return Err(r),
        Err(e) => return Err(Box::new(failed("config_save_failed", "saving config", &e.to_string()))),
    }
    tracing::info!(
        plugin_id = %result.id,
        plugin_name = %plugin.name,
        source_id = %source.id,
        version = %result.version,
        install_type = %result.install_type,
        path = %result.path,
        overwritten = result.overwritten,
        "pluginstore: plugin installed"
    );
    Ok(struct_json(
        StatusCode::OK,
        &json!({
            "status": "installed",
            "source_id": esc(&source.id),
            "source_name": esc(&source.name),
            "source_url": esc(&source.url),
            "id": esc(&result.id),
            "version": esc(&result.version),
            "install_type": esc(&result.install_type),
            "path": esc(&result.path),
            "plugins_enabled": snap.view.enabled,
            "restart_required": false,
        }),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requested_version_follows_go() {
        let q = |v: &str| vec![("version".to_owned(), v.to_owned())];
        assert_eq!(requested_version(&q(" v1.2 "), b"").unwrap(), "v1.2");
        assert_eq!(requested_version(&[], br#"{"version":" 1.3 "}"#).unwrap(), "1.3");
        // The query wins when both name the same version, spelled either way.
        assert_eq!(requested_version(&q("v1.2"), br#"{"version":"1.2"}"#).unwrap(), "v1.2");
        assert_eq!(
            requested_version(&q("1.2"), br#"{"version":"1.3"}"#).unwrap_err(),
            r#"version query "1.2" does not match request body version "1.3""#
        );
        assert_eq!(
            requested_version(&[], br#"{"version":1}"#).unwrap_err(),
            "decode install request: json: cannot unmarshal number into Go struct field pluginInstallRequest.version of type string"
        );
        // json.Unmarshal, not a Decoder: trailing data is an error.
        assert!(requested_version(&[], br#"{"version":"1"} x"#).is_err());
    }

    #[test]
    fn tag_candidates_try_both_spellings() {
        assert_eq!(release_tag_candidates(" V1.0 "), ["V1.0", "1.0"]);
        assert_eq!(release_tag_candidates("1.0"), ["1.0", "v1.0"]);
        assert!(release_tag_candidates(" ").is_empty());
    }

    #[test]
    fn source_status_matches_go() {
        let sources = vec![
            Source {
                id: "a".into(),
                name: "A".into(),
                url: "https://a/r.json".into(),
            },
            Source {
                id: "b".into(),
                name: "B".into(),
                url: "https://b/r.json".into(),
            },
        ];
        let fresh = LocalStatus::default();
        assert_eq!(
            install_source_status(&fresh, &sources, "a", 2),
            (String::new(), "", true)
        );
        let by_url = LocalStatus {
            installed: true,
            store_managed: true,
            installed_source_url: "https://b/r.json".into(),
            ..Default::default()
        };
        assert_eq!(
            install_source_status(&by_url, &sources, "a", 1),
            ("b".into(), "different", false)
        );
        assert_eq!(
            install_source_status(&by_url, &sources, "b", 1),
            ("b".into(), "matched", true)
        );
        // A known ID whose URL changed cannot be verified.
        let moved = LocalStatus {
            installed: true,
            store_managed: true,
            installed_source_id: "a".into(),
            installed_source_url: "https://elsewhere/r.json".into(),
            ..Default::default()
        };
        assert_eq!(
            install_source_status(&moved, &sources, "a", 1),
            (String::new(), "unknown", false)
        );
        // An unmanaged install is assumed to come from the only source listing it.
        let manual = LocalStatus {
            installed: true,
            ..Default::default()
        };
        assert_eq!(
            install_source_status(&manual, &sources, "a", 1),
            (String::new(), "assumed", true)
        );
        assert_eq!(
            install_source_status(&manual, &sources, "a", 2),
            (String::new(), "unknown", false)
        );
    }
}
