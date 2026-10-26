//! The plugin host lifecycle (internal/pluginhost/host.go, snapshot.go).
//!
//! [`Host::apply_config`] discovers plugin files, loads new ones (`dlopen` and
//! `plugin.register`), reconfigures loaded ones (`plugin.reconfigure`), hot-reloads a
//! changed file (`plugin.quiesce` on the old instance, rollback when the new one fails,
//! the old instance retired but still loaded) and publishes an immutable snapshot of
//! active plugins ordered by priority (descending) then ID. Disabling plugins globally
//! empties the snapshot but keeps libraries loaded, as Go does. A plugin whose host-side
//! adapter panicked is fused: skipped until its file changes.

use cpa_core::gostr::GoStr;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use bytes::Bytes;
use cpa_core::config::Config;

use crate::abi::method;
use crate::api;
use crate::callbacks::Callbacks;
use crate::client::{CallbackHandler, CallbackInstance, GuardedClient, PluginClient};
use crate::config::{self, ItemConfig};
use crate::gojson::GoJson;
use crate::platform::{self, PluginFile};
use crate::rpc::{self, CallError, Plugin};

/// Opens plugin libraries. The production loader is [`NativeLoader`].
pub trait Loader: Send + Sync + 'static {
    fn open(
        &self,
        file: &PluginFile,
        handler: Arc<dyn CallbackHandler>,
        instance: Arc<CallbackInstance>,
    ) -> Result<Arc<dyn PluginClient>, String>;
}

/// `dlopen`-based loader (Unix). Other platforms report plugins as unsupported.
pub struct NativeLoader;

impl Loader for NativeLoader {
    #[cfg(unix)]
    fn open(
        &self,
        file: &PluginFile,
        handler: Arc<dyn CallbackHandler>,
        instance: Arc<CallbackInstance>,
    ) -> Result<Arc<dyn PluginClient>, String> {
        crate::native::NativeClient::open(&file.path, &file.id, handler, instance)
            .map(|c| Arc::new(c) as Arc<dyn PluginClient>)
    }

    // ponytail: Go's Windows loader shadow-copies the DLL and calls through
    // syscall.NewCallback; not ported. Same error as Go builds without cgo.
    #[cfg(not(unix))]
    fn open(
        &self,
        file: &PluginFile,
        _: Arc<dyn CallbackHandler>,
        _: Arc<CallbackInstance>,
    ) -> Result<Arc<dyn PluginClient>, String> {
        Err(format!(
            "standard dynamic library plugin loading requires cgo on this platform: {}",
            file.path.display()
        ))
    }
}

/// Go `SupportPluginHeaderValue`: `1` when this build can load native plugins.
pub fn support_plugin_header_value() -> &'static str {
    crate::SUPPORT_PLUGIN
}

pub(crate) struct LoadedPlugin {
    pub id: String,
    pub path: PathBuf,
    pub version: String,
    pub name: String,
    pub config_yaml: Vec<u8>,
    pub plugin: Option<Plugin>,
    pub registered: bool,
    pub client: Arc<GuardedClient>,
}

/// One active plugin in a snapshot (Go `capabilityRecord`).
#[derive(Clone)]
pub struct Record {
    pub id: String,
    pub path: PathBuf,
    pub version: String,
    pub priority: i64,
    pub plugin: Plugin,
    pub client: Arc<GuardedClient>,
}

impl std::fmt::Debug for Record {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Record")
            .field("id", &self.id)
            .field("version", &self.version)
            .field("priority", &self.priority)
            .finish_non_exhaustive()
    }
}

/// Active plugins, highest priority first.
#[derive(Debug, Default)]
pub struct Snapshot {
    pub enabled: bool,
    pub records: Vec<Record>,
    /// `quota.describe` supported providers per plugin, asked once per snapshot.
    pub(crate) quota_supported: Mutex<HashMap<String, Vec<String>>>,
}

/// Go `RegisteredPluginInfo`.
#[derive(Debug, Clone, PartialEq)]
pub struct RegisteredPluginInfo {
    pub id: String,
    pub priority: i64,
    pub metadata: api::PluginMetadata,
    pub supports_oauth: bool,
    pub oauth_provider: String,
    pub supports_quota: bool,
    pub quota_provider: String,
    pub menus: Vec<RegisteredPluginMenu>,
}

/// Go `RegisteredPluginMenu`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegisteredPluginMenu {
    pub path: String,
    pub menu: String,
    pub description: String,
}

#[derive(Default)]
pub(crate) struct State {
    pub loaded: HashMap<String, LoadedPlugin>,
    pub retired: HashMap<String, Vec<LoadedPlugin>>,
    pub loading: HashSet<String>,
    pub fused: HashMap<String, String>,
    pub file_versions: HashMap<PathBuf, String>,
    pub active_versions: HashMap<String, String>,
    pub active_paths: HashMap<String, PathBuf>,
    pub cleanup_files_pending: bool,
    pub config: Option<Arc<Config>>,
    pub management_routes: BTreeMap<String, crate::management::RouteRecord>,
    pub resource_routes: BTreeMap<String, crate::management::ResourceRecord>,
    /// Thinking provider name to owning plugin ID.
    pub thinking_providers: BTreeMap<String, String>,
    /// Go `modelProviders`: plugin ID to the provider its models registered under.
    pub model_providers: HashMap<String, String>,
    pub model_registrations: HashMap<String, crate::models::ModelRegistration>,
    pub executor_providers: HashSet<String>,
    pub executor_model_client_ids: HashSet<String>,
    pub provider_models: BTreeMap<String, Vec<api::ModelInfo>>,
    /// Go `commandLineFlags` and `commandLineHits`.
    pub command_line_flags: BTreeMap<String, crate::cli::CliFlag>,
    pub command_line_hits: HashSet<String>,
}

pub(crate) struct Inner {
    apply: tokio::sync::Mutex<()>,
    pub state: Mutex<State>,
    snapshot: RwLock<Arc<Snapshot>>,
    loader: Arc<dyn Loader>,
    pub callbacks: Callbacks,
    /// Set once this host's translator hooks are handed out (`translator_hooks`): from
    /// then on its lifecycle keeps cpa-translate's hook flag current.
    pub(crate) hooks_installed: std::sync::atomic::AtomicBool,
    /// The same for the auth parser (`auth_parser`) and cpa-core's auth-parser flag.
    pub(crate) auth_parser_installed: std::sync::atomic::AtomicBool,
    /// What `auth_parser` hands out.
    pub(crate) auth_parser: std::sync::OnceLock<Arc<dyn cpa_core::config::credentials::PluginAuthParser>>,
    /// The `auth.parse` calls in flight: the plugin instance asked, the auth file's
    /// cleaned absolute path, and whether a `host.auth.save` asked (`auth::Parsing`).
    pub(crate) auth_parses: Mutex<Vec<(Arc<CallbackInstance>, PathBuf, bool)>>,
}

/// The plugin host. Cheap to clone.
#[derive(Clone)]
pub struct Host {
    pub(crate) inner: Arc<Inner>,
}

pub(crate) fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Go `cleanPluginPath`.
fn clean_path(path: &Path) -> PathBuf {
    if path.as_os_str().is_empty() {
        PathBuf::new()
    } else {
        platform::clean(path)
    }
}

/// Go `normalizeProviderID`.
pub fn normalize_provider(provider: &str) -> String {
    use cpa_common::gostr::GoStr;
    provider.trim().go_lower()
}

impl Default for Host {
    fn default() -> Self {
        Self::new()
    }
}

impl Host {
    pub fn new() -> Self {
        Self::with_loader(Arc::new(NativeLoader))
    }

    pub fn with_loader(loader: Arc<dyn Loader>) -> Self {
        let inner = Arc::new_cyclic(|me| Inner {
            apply: tokio::sync::Mutex::new(()),
            state: Mutex::new(State {
                cleanup_files_pending: true,
                ..Default::default()
            }),
            snapshot: RwLock::new(Arc::default()),
            loader,
            callbacks: Callbacks::new(me.clone()),
            hooks_installed: Default::default(),
            auth_parser_installed: Default::default(),
            auth_parser: Default::default(),
            auth_parses: Default::default(),
        });
        Self { inner }
    }

    pub(crate) fn from_inner(inner: Arc<Inner>) -> Self {
        Self { inner }
    }

    pub fn snapshot(&self) -> Arc<Snapshot> {
        self.inner
            .snapshot
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// Publishes a snapshot. Takes the state guard so that publication and the active
    /// maps change in one critical section (Go `h.mu`): a registration that compares
    /// snapshots under the state lock then never commits against a half-applied change.
    fn store_snapshot(&self, state: &mut State, snapshot: Snapshot) {
        self.publish_hook_activity(state, &snapshot.records);
        *self
            .inner
            .snapshot
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Arc::new(snapshot);
    }

    pub(crate) fn state(&self) -> std::sync::MutexGuard<'_, State> {
        lock(&self.inner.state)
    }

    /// The config last applied.
    pub fn config(&self) -> Option<Arc<Config>> {
        self.state().config.clone()
    }

    /// Records still current: a record whose plugin was unloaded or replaced since the
    /// snapshot was taken is skipped (Go `activeRecordsFromSnapshot`).
    pub fn active_records(&self) -> Vec<Record> {
        self.current_records(&self.snapshot())
    }

    /// Go `activeRecordsFromSnapshot`: the snapshot's records that are still current.
    pub fn current_records(&self, snapshot: &Snapshot) -> Vec<Record> {
        snapshot
            .records
            .iter()
            .filter(|r| self.record_current(r))
            .cloned()
            .collect()
    }

    /// Whether a record may be called right now: not fused and still current. Go checks
    /// this at every call, after earlier plugins in the same loop have run.
    pub fn live(&self, record: &Record) -> bool {
        !self.is_fused(&record.id) && self.record_current(record)
    }

    /// Go `recordCurrent` / `pluginIdentityCurrent`.
    pub fn record_current(&self, record: &Record) -> bool {
        let state = self.state();
        let id = record.id.trim();
        let path = clean_path(&record.path);
        let version = record.version.trim();
        !id.is_empty()
            && !path.as_os_str().is_empty()
            && state.active_paths.get(id) == Some(&path)
            && state.file_versions.get(&path).map(String::as_str) == Some(version)
            && state.active_versions.get(id).map(String::as_str) == Some(version)
    }

    pub fn is_fused(&self, id: &str) -> bool {
        self.state().fused.contains_key(id)
    }

    /// Go `fusePlugin`: a host-side panic while serving the plugin disables it until its
    /// file changes.
    pub fn fuse(&self, id: &str, what: &str, panic: &str) {
        let mut state = self.state();
        state.fused.insert(id.to_owned(), format!("{what} panic: {panic}"));
        // Go `thinking.UnregisterPluginProviders`.
        state.thinking_providers.retain(|_, owner| owner != id);
        cpa_common::thinking::unregister_plugin_providers(id);
        self.publish_hook_activity(&state, &self.snapshot().records);
        drop(state);
        tracing::error!(plugin_id = %id, method = %what, "pluginhost: plugin panic recovered: {panic}");
    }

    /// Fuses the plugin when `result` is a panic, passing the result through.
    pub(crate) fn fuse_on_panic<T>(&self, id: &str, what: &str, result: Result<T, CallError>) -> Result<T, CallError> {
        if let Err(CallError::Panic(p)) = &result {
            self.fuse(id, what, p);
        }
        result
    }

    /// Go `PluginLoaded`: a library for `id` is still loaded (active or retired).
    pub fn plugin_loaded(&self, id: &str) -> bool {
        let id = id.trim();
        let state = self.state();
        !id.is_empty() && (state.loaded.contains_key(id) || state.retired.get(id).is_some_and(|r| !r.is_empty()))
    }

    /// Go `PluginBusy`: loaded or still loading.
    pub fn plugin_busy(&self, id: &str) -> bool {
        self.plugin_loaded(id) || self.state().loading.contains(id.trim())
    }

    /// Go `PluginRegistered`: active in the current snapshot.
    pub fn plugin_registered(&self, id: &str) -> bool {
        let id = id.trim();
        !id.is_empty() && self.active_records().iter().any(|r| r.id == id)
    }

    /// Go `RegisteredPlugins`.
    pub fn registered_plugins(&self) -> Vec<RegisteredPluginInfo> {
        let records = self.active_records();
        let menus = self.registered_menus();
        records
            .iter()
            .map(|r| {
                let fused = self.is_fused(&r.id);
                RegisteredPluginInfo {
                    id: r.id.clone(),
                    priority: r.priority,
                    metadata: r.plugin.metadata.clone(),
                    supports_oauth: r.plugin.caps.auth_provider,
                    oauth_provider: if r.plugin.caps.auth_provider && !fused {
                        normalize_provider(&r.plugin.auth_identifier)
                    } else {
                        String::new()
                    },
                    supports_quota: r.plugin.caps.quota_provider,
                    quota_provider: if r.plugin.caps.quota_provider && !fused {
                        normalize_provider(&r.plugin.quota_identifier)
                    } else {
                        String::new()
                    },
                    menus: menus.get(&r.id).cloned().unwrap_or_default(),
                }
            })
            .collect()
    }

    fn registered_menus(&self) -> HashMap<String, Vec<RegisteredPluginMenu>> {
        let state = self.state();
        let mut out: HashMap<String, Vec<RegisteredPluginMenu>> = HashMap::new();
        for record in state.resource_routes.values() {
            let menu = record.route.menu.trim();
            if menu.is_empty() {
                continue;
            }
            out.entry(record.plugin_id.clone())
                .or_default()
                .push(RegisteredPluginMenu {
                    path: record.route.path.trim().to_owned(),
                    menu: menu.to_owned(),
                    description: record.route.description.trim().to_owned(),
                });
        }
        for menus in out.values_mut() {
            menus.sort_by(|a, b| a.path.cmp(&b.path));
        }
        out
    }

    /// Go `ApplyConfig`.
    ///
    /// ponytail: runs to completion on its own task even if the caller stops waiting, so
    /// a dropped future can never strand a half-loaded plugin; Go instead aborts on
    /// context cancellation and rolls the load back. Add cancellation with rollback if
    /// shutdown latency during a slow `plugin.register` ever matters.
    pub async fn apply_config(&self, cfg: Arc<Config>) {
        let host = self.clone();
        let _ = tokio::spawn(async move { host.apply_config_inner(cfg).await }).await;
    }

    async fn apply_config_inner(&self, cfg: Arc<Config>) {
        let _apply = self.inner.apply.lock().await;
        let rc = match config::runtime_config(&cfg.document, &cfg.plugin_configs) {
            Ok(rc) => rc,
            Err(e) => {
                tracing::error!("failed to apply plugin runtime config: {e}");
                return;
            }
        };
        self.state().config = Some(cfg.clone());
        if !rc.enabled {
            self.deactivate_all();
            self.refresh_thinking_providers().await;
            return;
        }
        let desired = config::desired_versions(&rc.items);
        let files = match platform::discover(&rc.dir, &desired) {
            Ok(files) => files,
            Err(e) => {
                tracing::warn!("pluginhost: failed to select plugin files: {e}");
                self.deactivate_all();
                self.refresh_thinking_providers().await;
                return;
            }
        };
        let files = self.with_loaded_fallbacks(files, &rc.items, &desired);

        let mut records = Vec::new();
        let mut loaded_files = Vec::new();
        let mut hot_reloads = Vec::new();
        for file in files {
            let item = rc
                .items
                .get(&file.id)
                .cloned()
                .unwrap_or_else(|| ItemConfig::default_for(&file.id));
            if !item.enabled {
                continue;
            }
            let (existing, replaced, fused) = {
                let state = self.state();
                let existing = state
                    .loaded
                    .get(&file.id)
                    .map(|lp| (lp.path.clone(), lp.version.clone()));
                let replaced = existing
                    .as_ref()
                    .is_some_and(|(path, _)| clean_path(path) != clean_path(&file.path));
                (existing, replaced, state.fused.contains_key(&file.id))
            };
            if fused && !replaced {
                continue;
            }
            let plugin = if existing.is_none() || replaced {
                if !self.state().loading.insert(file.id.clone()) {
                    continue;
                }
                if replaced {
                    self.quiesce(&file.id).await;
                }
                let loaded = self.load(&file, &item).await;
                let (lp, plugin) = match loaded {
                    Err(e) => {
                        self.state().loading.remove(&file.id);
                        if replaced {
                            self.rollback_into(&file.id, &item, &mut records, &mut loaded_files)
                                .await;
                        }
                        tracing::warn!(
                            "pluginhost: failed to load plugin {} from {}: {e}",
                            file.id,
                            file.path.display()
                        );
                        continue;
                    }
                    Ok(pair) => pair,
                };
                if replaced && plugin.is_none() {
                    self.discard(lp).await;
                    self.state().loading.remove(&file.id);
                    self.rollback_into(&file.id, &item, &mut records, &mut loaded_files)
                        .await;
                    continue;
                }
                {
                    let mut state = self.state();
                    state.loading.remove(&file.id);
                    if replaced && let Some(old) = state.loaded.remove(&file.id) {
                        hot_reloads.push((
                            file.id.clone(),
                            file.version.clone(),
                            file.path.clone(),
                            old.version.clone(),
                            old.path.clone(),
                        ));
                        state.retired.entry(file.id.clone()).or_default().push(old);
                        state.fused.remove(&file.id);
                        remove_runtime_state(&mut state, &file.id);
                        // The unfused plugin is eligible now, not once the remaining
                        // plugins have registered and the snapshot is published.
                        self.publish_hook_activity(&state, &self.snapshot().records);
                    }
                    state.loaded.insert(file.id.clone(), lp);
                }
                tracing::info!(plugin_id = %file.id, version = %file.version, path = %file.path.display(), "pluginhost: plugin loaded");
                match plugin {
                    Some(plugin) => {
                        tracing::info!(plugin_id = %file.id, plugin_name = %plugin.metadata.name, version = %plugin.metadata.version, path = %file.path.display(), "pluginhost: plugin registered");
                        plugin
                    }
                    // Loaded but not registered: stays loaded, retried on the next apply.
                    None => continue,
                }
            } else {
                match self.call_register(&file.id, &item).await {
                    Some(plugin) => plugin,
                    None => continue,
                }
            };
            records.push(Record {
                id: file.id.clone(),
                path: file.path.clone(),
                version: file.version.clone(),
                priority: item.priority,
                client: self.client_of(&file.id).expect("loaded above"),
                plugin,
            });
            loaded_files.push(file);
        }

        sort_records(&mut records);
        let cleanup = {
            let mut state = self.state();
            let cleanup = state.cleanup_files_pending && !loaded_files.is_empty();
            if !loaded_files.is_empty() {
                state.cleanup_files_pending = false;
            }
            rebuild_active_maps(&mut state, &records);
            self.store_snapshot(
                &mut state,
                Snapshot {
                    enabled: true,
                    records,
                    ..Default::default()
                },
            );
            cleanup
        };
        self.refresh_thinking_providers().await;
        for (id, version, path, old_version, old_path) in hot_reloads {
            tracing::info!(plugin_id = %id, active_version = %version, active_path = %path.display(), retired_version = %old_version, retired_path = %old_path.display(), "pluginhost: plugin hot reloaded");
        }
        if cleanup && let Err(e) = platform::cleanup_unselected(&rc.dir, &loaded_files) {
            tracing::warn!("pluginhost: failed to clean old plugin files: {e}");
        }
    }

    fn deactivate_all(&self) {
        let mut state = self.state();
        state.management_routes.clear();
        state.resource_routes.clear();
        rebuild_active_maps(&mut state, &[]);
        self.store_snapshot(&mut state, Snapshot::default());
    }

    fn client_of(&self, id: &str) -> Option<Arc<GuardedClient>> {
        self.state().loaded.get(id).map(|lp| lp.client.clone())
    }

    /// Go `withLoadedPluginFallbacks`: keep running the loaded version of a plugin whose
    /// desired version has no file yet.
    fn with_loaded_fallbacks(
        &self,
        mut files: Vec<PluginFile>,
        items: &BTreeMap<String, ItemConfig>,
        desired: &BTreeMap<String, String>,
    ) -> Vec<PluginFile> {
        let mut selected: HashSet<String> = files.iter().map(|f| f.id.trim().to_owned()).collect();
        let state = self.state();
        for id in desired.keys() {
            if selected.contains(id) || items.get(id).is_some_and(|item| !item.enabled) {
                continue;
            }
            let Some(lp) = state.loaded.get(id) else {
                continue;
            };
            if lp.path.as_os_str().is_empty() {
                continue;
            }
            files.push(PluginFile {
                id: id.clone(),
                path: lp.path.clone(),
                version: lp.version.trim().to_owned(),
            });
            selected.insert(id.clone());
        }
        files
    }

    /// Opens and registers one file. `Ok((lp, None))` is a loaded library whose
    /// registration failed or was invalid.
    async fn load(&self, file: &PluginFile, item: &ItemConfig) -> Result<(LoadedPlugin, Option<Plugin>), String> {
        let instance = Arc::new(CallbackInstance::default());
        let handler: Arc<dyn CallbackHandler> = Arc::new(self.inner.callbacks.handler());
        let loader = self.inner.loader.clone();
        let (file_owned, instance_owned) = (file.clone(), instance.clone());
        let client = tokio::task::spawn_blocking(move || loader.open(&file_owned, handler, instance_owned))
            .await
            .map_err(|e| format!("plugin loader panic: {e}"))??;
        let client = GuardedClient::new(client, instance);
        let mut lp = LoadedPlugin {
            id: file.id.clone(),
            path: file.path.clone(),
            version: file.version.clone(),
            name: String::new(),
            config_yaml: Vec::new(),
            plugin: None,
            registered: false,
            client,
        };
        let plugin = self.register_loaded(&mut lp, item).await;
        Ok((lp, plugin))
    }

    /// Go `callRegister` on a plugin not yet in the loaded map.
    async fn register_loaded(&self, lp: &mut LoadedPlugin, item: &ItemConfig) -> Option<Plugin> {
        let method = if lp.registered {
            method::PLUGIN_RECONFIGURE
        } else {
            method::PLUGIN_REGISTER
        };
        let result = rpc::register(&lp.client, method, &item.config_yaml).await;
        if let Err(CallError::Panic(p)) = &result {
            self.fuse(&lp.id, method, p);
            return None;
        }
        lp.registered = true;
        let plugin = match result {
            Ok(plugin) if plugin.is_valid() => plugin,
            Ok(_) => {
                tracing::warn!(
                    "pluginhost: plugin {} returned invalid metadata or no capabilities",
                    lp.id
                );
                return None;
            }
            Err(e) => {
                tracing::warn!("pluginhost: plugin {} {method} failed: {e}", lp.id);
                tracing::warn!(
                    "pluginhost: plugin {} returned invalid metadata or no capabilities",
                    lp.id
                );
                return None;
            }
        };
        lp.name = plugin.metadata.name.trim().to_owned();
        if lp.version.trim().is_empty() {
            lp.version = plugin.metadata.version.trim().to_owned();
        }
        lp.config_yaml = item.config_yaml.clone();
        lp.plugin = Some(plugin.clone());
        Some(plugin)
    }

    /// Go `callRegister` for a plugin already in the loaded map.
    async fn call_register(&self, id: &str, item: &ItemConfig) -> Option<Plugin> {
        let (client, registered) = {
            let state = self.state();
            let lp = state.loaded.get(id)?;
            (lp.client.clone(), lp.registered)
        };
        let mut scratch = LoadedPlugin {
            id: id.to_owned(),
            path: PathBuf::new(),
            version: String::new(),
            name: String::new(),
            config_yaml: Vec::new(),
            plugin: None,
            registered,
            client,
        };
        let plugin = self.register_loaded(&mut scratch, item).await;
        let mut state = self.state();
        if let Some(lp) = state.loaded.get_mut(id) {
            lp.registered |= scratch.registered;
            if let Some(plugin) = &plugin {
                lp.name = scratch.name;
                if lp.version.trim().is_empty() {
                    lp.version = plugin.metadata.version.trim().to_owned();
                }
                lp.config_yaml = scratch.config_yaml;
                lp.plugin = Some(plugin.clone());
            }
        }
        plugin
    }

    async fn rollback_into(&self, id: &str, item: &ItemConfig, records: &mut Vec<Record>, files: &mut Vec<PluginFile>) {
        if let Some((record, file)) = self.rollback_replacement(id, item).await {
            records.push(record);
            files.push(file);
        }
    }

    /// Go `rollbackReplacement`: re-register the still-loaded old instance with its last
    /// config; keep its previous registration when that fails.
    async fn rollback_replacement(&self, id: &str, item: &ItemConfig) -> Option<(Record, PluginFile)> {
        let (path, version, previous, config_yaml) = {
            let state = self.state();
            let lp = state.loaded.get(id)?;
            (
                lp.path.clone(),
                lp.version.clone(),
                lp.plugin.clone(),
                lp.config_yaml.clone(),
            )
        };
        let mut item = item.clone();
        if !config_yaml.is_empty() {
            item.config_yaml = config_yaml;
        }
        let plugin = match self.call_register(id, &item).await {
            Some(plugin) => {
                let mut state = self.state();
                state.fused.remove(id);
                self.publish_hook_activity(&state, &self.snapshot().records);
                plugin
            }
            None => previous?,
        };
        if !plugin.is_valid() {
            return None;
        }
        Some((
            Record {
                id: id.to_owned(),
                path: path.clone(),
                version: version.clone(),
                priority: item.priority,
                client: self.client_of(id)?,
                plugin,
            },
            PluginFile {
                id: id.to_owned(),
                path,
                version,
            },
        ))
    }

    /// Go `callQuiesce`, before a replacement loads.
    async fn quiesce(&self, id: &str) -> bool {
        let Some(client) = self.client_of(id) else {
            return false;
        };
        let result: Result<rpc::Empty, _> = rpc::call(&client, method::PLUGIN_QUIESCE, &rpc::Empty {}).await;
        match self.fuse_on_panic(id, method::PLUGIN_QUIESCE, result) {
            Ok(_) => true,
            Err(e) => {
                if quiesce_unsupported(&e) {
                    tracing::debug!(plugin_id = %id, "pluginhost: plugin quiesce unsupported: {e}");
                } else {
                    tracing::warn!(plugin_id = %id, "pluginhost: plugin quiesce failed: {e}");
                }
                false
            }
        }
    }

    async fn discard(&self, lp: LoadedPlugin) {
        self.inner.callbacks.close_instance(&lp.id, lp.client.instance());
        lp.client.shutdown(None).await;
    }

    /// Go `UnloadPluginContext`: detach `id` from the runtime, then close every loaded
    /// instance of it, waiting up to `wait` for running calls.
    pub async fn unload_plugin(&self, id: &str, wait: Option<Duration>) -> bool {
        self.spawn_unload(id, wait, None).await.unwrap_or(false)
    }

    /// Go `UnloadPluginContext` with a cancellable context: cancelling `cancel` stops
    /// waiting for the apply lock (`false`) or for the plugin to shut down, and releases
    /// the lock; the shutdown itself still completes on its own thread.
    pub async fn unload_plugin_until(&self, id: &str, cancel: &tokio_util::sync::CancellationToken) -> bool {
        self.spawn_unload(id, None, Some(cancel.clone())).await.unwrap_or(false)
    }

    /// Not inlined, so callers in other crates share one instance of the spawned unload
    /// instead of compiling their own (about 25 KB each in the release binary).
    #[inline(never)]
    fn spawn_unload(
        &self,
        id: &str,
        wait: Option<Duration>,
        cancel: Option<tokio_util::sync::CancellationToken>,
    ) -> tokio::task::JoinHandle<bool> {
        let host = self.clone();
        let id = id.to_owned();
        tokio::spawn(async move { host.unload_plugin_inner(&id, wait, cancel).await })
    }

    async fn unload_plugin_inner(
        &self,
        id: &str,
        wait: Option<Duration>,
        cancel: Option<tokio_util::sync::CancellationToken>,
    ) -> bool {
        let id = id.trim();
        if id.is_empty() {
            return false;
        }
        // A fresh future for each wait: one that completed must not be polled again.
        let cancelled = || async {
            match &cancel {
                Some(cancel) => cancel.cancelled().await,
                None => std::future::pending().await,
            }
        };
        // Go `lockApply`: an available lock is taken even when already cancelled.
        let _apply = tokio::select! {
            biased;
            apply = self.inner.apply.lock() => apply,
            () = cancelled() => return false,
        };
        let targets = {
            let mut state = self.state();
            let mut targets: Vec<LoadedPlugin> = state.loaded.remove(id).into_iter().collect();
            targets.extend(state.retired.remove(id).unwrap_or_default());
            let loading = state.loading.contains(id);
            if targets.is_empty() && !loading {
                return false;
            }
            state.fused.remove(id);
            state.active_versions.remove(id);
            state.active_paths.remove(id);
            for target in &targets {
                state.file_versions.remove(&clean_path(&target.path));
            }
            remove_runtime_state(&mut state, id);
            #[cfg(test)]
            tests::unload_hook(id);
            let snapshot = self.snapshot();
            let records = snapshot.records.iter().filter(|r| r.id != id).cloned().collect();
            self.store_snapshot(
                &mut state,
                Snapshot {
                    enabled: snapshot.enabled,
                    records,
                    ..Default::default()
                },
            );
            targets
        };
        self.inner.callbacks.close_plugin(id);
        self.refresh_thinking_providers().await;
        for target in targets {
            let (name, version, path) = (target.name.clone(), target.version.clone(), target.path.clone());
            self.inner.callbacks.close_instance(id, target.client.instance());
            // The first poll detaches the client and starts its shutdown thread, also
            // once cancelled.
            tokio::select! {
                biased;
                () = target.client.shutdown(wait) => {}
                () = cancelled() => {}
            }
            tracing::info!(plugin_id = %id, plugin_name = %name, version = %version, path = %path.display(), "pluginhost: plugin unloaded");
        }
        true
    }

    /// Go `ShutdownAllContext`.
    pub async fn shutdown_all(&self, wait: Option<Duration>) {
        let host = self.clone();
        let _ = tokio::spawn(async move { host.shutdown_all_inner(wait).await }).await;
    }

    async fn shutdown_all_inner(&self, wait: Option<Duration>) {
        let _apply = self.inner.apply.lock().await;
        let targets: Vec<LoadedPlugin> = {
            let mut state = self.state();
            let mut targets: Vec<LoadedPlugin> = state.loaded.drain().map(|(_, lp)| lp).collect();
            targets.extend(state.retired.drain().flat_map(|(_, lps)| lps));
            state.management_routes.clear();
            state.resource_routes.clear();
            state.executor_model_client_ids.clear();
            state.model_providers.clear();
            state.model_registrations.clear();
            state.provider_models.clear();
            state.executor_providers.clear();
            state.command_line_flags.clear();
            state.command_line_hits.clear();
            state.file_versions.clear();
            state.active_versions.clear();
            state.active_paths.clear();
            self.store_snapshot(&mut state, Snapshot::default());
            targets
        };
        self.state().thinking_providers.clear();
        cpa_common::thinking::clear_plugin_providers();
        self.inner.callbacks.close_all();
        for target in targets {
            self.inner
                .callbacks
                .close_instance(&target.id, target.client.instance());
            target.client.shutdown(wait).await;
            tracing::info!(plugin_id = %target.id, plugin_name = %target.name, version = %target.version, path = %target.path.display(), "pluginhost: plugin unloaded");
        }
    }

    /// Looks up an active, unfused record by plugin ID.
    pub fn record(&self, id: &str) -> Option<Record> {
        self.active_records()
            .into_iter()
            .find(|r| r.id == id && !self.is_fused(&r.id))
    }

    /// One typed RPC to an active plugin, fusing it on a host-side panic.
    pub async fn call<T: GoJson, R: GoJson>(&self, record: &Record, method: &str, request: &R) -> Result<T, CallError> {
        let result = rpc::call(&record.client, method, request).await;
        self.fuse_on_panic(&record.id, method, result)
    }

    /// One RPC that lets the plugin call back: opens a callback context for the record's
    /// instance, sends its ID as `host_callback_id`, and closes it when the call returns
    /// (Go `openHostCallbackContext` around adapter calls).
    pub async fn call_with_callback<T: GoJson, R: crate::gojson::GoStruct>(
        &self,
        record: &Record,
        method: &str,
        request: &R,
        scope: &crate::callbacks::RequestScope,
    ) -> Result<T, CallError> {
        let guard = self.open_callback(record, scope);
        let raw = rpc::encode_with_callback(request, guard.id());
        let result = self.call_raw(record, method, raw).await;
        drop(guard);
        result
    }

    /// [`Host::call`] for synchronous callers; see [`GuardedClient::call_blocking`].
    pub fn call_blocking<T: GoJson, R: GoJson>(
        &self,
        record: &Record,
        method: &str,
        request: &R,
    ) -> Result<T, CallError> {
        let result = rpc::call_raw_blocking(&record.client, method, &crate::gojson::to_vec(request));
        self.fuse_on_panic(&record.id, method, result)
    }

    /// [`Host::call_with_callback`] for synchronous callers.
    pub fn call_with_callback_blocking<T: GoJson, R: crate::gojson::GoStruct>(
        &self,
        record: &Record,
        method: &str,
        request: &R,
        scope: &crate::callbacks::RequestScope,
    ) -> Result<T, CallError> {
        let guard = self.open_callback(record, scope);
        let raw = rpc::encode_with_callback(request, guard.id());
        let result = rpc::call_raw_blocking(&record.client, method, &raw);
        drop(guard);
        self.fuse_on_panic(&record.id, method, result)
    }

    /// Opens a callback context for `record`'s instance.
    pub fn open_callback(
        &self,
        record: &Record,
        scope: &crate::callbacks::RequestScope,
    ) -> crate::callbacks::ContextGuard {
        self.inner
            .callbacks
            .open(&record.id, Some(record.client.instance().clone()), scope.clone())
    }

    /// One RPC with a pre-encoded request.
    pub async fn call_raw<T: GoJson>(&self, record: &Record, method: &str, request: Vec<u8>) -> Result<T, CallError> {
        let result = rpc::call_raw(&record.client, method, request).await;
        self.fuse_on_panic(&record.id, method, result)
    }
}

/// Go `quiesceUnsupported`.
fn quiesce_unsupported(e: &CallError) -> bool {
    if matches!(
        e.code().to_ascii_lowercase().as_str(),
        "unknown_method" | "method_not_found" | "unsupported_method"
    ) {
        return true;
    }
    let message = e.to_string().trim().go_lower();
    [
        "unknown method",
        "method not found",
        "unsupported method",
        "method unsupported",
        "method is not supported",
        "method not supported",
    ]
    .iter()
    .any(|needle| message.contains(needle))
}

/// Go `sortRecords`.
fn sort_records(records: &mut [Record]) {
    records.sort_by(|a, b| b.priority.cmp(&a.priority).then_with(|| a.id.cmp(&b.id)));
}

fn rebuild_active_maps(state: &mut State, records: &[Record]) {
    state.file_versions.clear();
    state.active_versions.clear();
    state.active_paths.clear();
    for record in records {
        let id = record.id.trim();
        let path = clean_path(&record.path);
        if id.is_empty() || path.as_os_str().is_empty() {
            continue;
        }
        state
            .file_versions
            .insert(path.clone(), record.version.trim().to_owned());
        state
            .active_versions
            .insert(id.to_owned(), record.version.trim().to_owned());
        state.active_paths.insert(id.to_owned(), path);
    }
}

/// Go `removePluginRuntimeStateLocked`.
fn remove_runtime_state(state: &mut State, id: &str) {
    state.management_routes.retain(|_, r| r.plugin_id != id);
    state.resource_routes.retain(|_, r| r.plugin_id != id);
    let flags = &mut state.command_line_flags;
    let hits = &mut state.command_line_hits;
    flags.retain(|name, f| {
        let keep = f.plugin_id != id;
        if !keep {
            hits.remove(name);
        }
        keep
    });
    if let Some(registration) = state.model_registrations.remove(id) {
        state.provider_models.remove(&registration.provider);
    }
    state.model_providers.remove(id);
}

/// Payload bytes for an RPC field that Go fills with `bytes.Clone`.
pub fn bytes_of(v: &[u8]) -> Bytes {
    Bytes::copy_from_slice(v)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    type Hook = Box<dyn Fn() + Send + Sync>;
    /// Keyed by plugin ID, so another test's unload never fires it.
    static UNLOAD_HOOK: Mutex<Option<(&'static str, Hook)>> = Mutex::new(None);

    /// Runs inside `unload_plugin` after its state change, before the snapshot is
    /// published.
    pub(super) fn unload_hook(id: &str) {
        if let Some((_, hook)) = lock(&UNLOAD_HOOK).as_ref().filter(|(target, _)| *target == id) {
            hook();
        }
    }

    struct OneLoader(Arc<dyn PluginClient>);

    impl Loader for OneLoader {
        fn open(
            &self,
            _: &PluginFile,
            _: Arc<dyn CallbackHandler>,
            _: Arc<CallbackInstance>,
        ) -> Result<Arc<dyn PluginClient>, String> {
            Ok(self.0.clone())
        }
    }

    /// Go publishes the active maps and the snapshot under one lock. Registrations
    /// compare snapshots and commit under the state lock, so while an unload has
    /// changed the state but not yet published its snapshot, that lock must be held:
    /// otherwise a registration that took the old snapshot commits stale results.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn unload_publishes_its_snapshot_inside_the_state_lock() {
        let dir = std::env::temp_dir().join(format!("cpa-plugin-fence-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(format!("fence{}", platform::extension(platform::goos()))), b"").unwrap();
        let host = Host::with_loader(Arc::new(OneLoader(Arc::new(FakeClient))));
        let yaml = format!(
            "plugins:\n  enabled: true\n  dir: {}\n  configs:\n    fence:\n      enabled: true\n",
            dir.display()
        );
        host.apply_config(Arc::new(Config::parse(&yaml).unwrap())).await;
        assert!(host.plugin_registered("fence"));
        // (snapshot still lists the plugin, state lock free) observed inside the window.
        let seen = Arc::new(Mutex::new(None));
        *lock(&UNLOAD_HOOK) = Some({
            let (host, seen) = (host.clone(), seen.clone());
            (
                "fence",
                Box::new(move || {
                    let stale = host.snapshot().records.iter().any(|r| r.id == "fence");
                    let lock_free = host.inner.state.try_lock().is_ok();
                    *lock(&seen) = Some((stale, lock_free));
                }),
            )
        });
        assert!(host.unload_plugin("fence", None).await);
        *lock(&UNLOAD_HOOK) = None;
        assert_eq!(
            *lock(&seen),
            Some((true, false)),
            "the old snapshot is visible only while the state lock is held"
        );
        assert!(host.snapshot().records.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// [`FakeClient`] whose shutdown counts its start, then blocks while `held` is set.
    struct HeldClient {
        held: Arc<(Mutex<bool>, std::sync::Condvar)>,
        started: Arc<AtomicUsize>,
    }

    impl PluginClient for HeldClient {
        fn call(&self, method: &str, request: &[u8]) -> Result<Bytes, String> {
            FakeClient.call(method, request)
        }
        fn shutdown(&self) {
            self.started.fetch_add(1, Ordering::SeqCst);
            let (held, cv) = &*self.held;
            let mut held = lock(held);
            while *held {
                held = cv.wait(held).unwrap();
            }
        }
    }

    async fn within<T>(what: &str, fut: impl std::future::Future<Output = T>) -> T {
        tokio::time::timeout(Duration::from_secs(5), fut)
            .await
            .unwrap_or_else(|_| panic!("timed out waiting for {what}"))
    }

    /// Go `UnloadPluginContext` from a Home delete (`deletePluginArtifact`): cancelling
    /// the delete while the plugin's shutdown hangs ends the unload and releases the
    /// apply lock, so a later config apply proceeds. Every instance, the loaded one and
    /// a retired one still draining, is detached and has its shutdown started.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_cancelled_home_unload_releases_the_host() {
        let dir = std::env::temp_dir().join(format!("cpa-plugin-held-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(format!("held{}", platform::extension(platform::goos()))), b"").unwrap();
        let held = Arc::new((Mutex::new(false), std::sync::Condvar::new()));
        let started = Arc::new(AtomicUsize::new(0));
        let client = || {
            Arc::new(HeldClient {
                held: held.clone(),
                started: started.clone(),
            })
        };
        let host = Host::with_loader(Arc::new(OneLoader(client())));
        let config = |enabled: bool| {
            let yaml = format!(
                "plugins:\n  enabled: {enabled}\n  dir: {}\n  configs:\n    held:\n      enabled: true\n",
                dir.display()
            );
            Arc::new(Config::parse(&yaml).unwrap())
        };
        host.apply_config(config(true)).await;
        assert!(host.plugin_registered("held"));
        let retired = LoadedPlugin {
            id: "held".into(),
            path: dir.join("held-old"),
            version: "0".into(),
            name: "n".into(),
            config_yaml: Vec::new(),
            plugin: None,
            registered: true,
            client: GuardedClient::new(client(), Arc::new(CallbackInstance::default())),
        };
        host.state().retired.entry("held".into()).or_default().push(retired);
        *lock(&held.0) = true;
        let cancel = tokio_util::sync::CancellationToken::new();
        let unload = tokio::spawn({
            let (host, cancel) = (host.clone(), cancel.clone());
            async move { crate::homeplugins::PluginRuntime::unload_plugin(&host, "held", &cancel).await }
        });
        within("the first shutdown", async {
            while started.load(Ordering::SeqCst) < 1 {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await;
        assert!(!host.plugin_registered("held"));
        assert!(
            host.inner.apply.try_lock().is_err(),
            "the hanging unload holds the host"
        );
        cancel.cancel();
        assert!(
            within("the cancelled unload", unload).await.unwrap(),
            "the cancelled unload panicked or reported nothing unloaded"
        );
        // The retired instance's shutdown runs on its own thread.
        within("the retired instance's shutdown", async {
            while started.load(Ordering::SeqCst) < 2 {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await;
        within(
            "a config apply after the cancelled unload",
            host.apply_config(config(false)),
        )
        .await;
        *lock(&held.0) = false;
        held.1.notify_all();
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A plugin that registers with one capability.
    struct FakeClient;

    impl PluginClient for FakeClient {
        fn call(&self, method: &str, _: &[u8]) -> Result<Bytes, String> {
            Ok(match method {
                method::PLUGIN_REGISTER | method::PLUGIN_RECONFIGURE => Bytes::from_static(
                    br#"{"ok":true,"result":{"schema_version":6,"metadata":{"Name":"n","Version":"1","Author":"a","GitHubRepository":"r"},"capabilities":{"usage_plugin":true}}}"#,
                ),
                _ => Bytes::from_static(br#"{"ok":true,"result":{}}"#),
            })
        }
        fn shutdown(&self) {}
    }

    /// Blocks `open` until released; counts opens.
    struct GateLoader {
        release: Arc<(Mutex<bool>, std::sync::Condvar)>,
        opens: AtomicUsize,
    }

    impl Loader for GateLoader {
        fn open(
            &self,
            _: &PluginFile,
            _: Arc<dyn CallbackHandler>,
            _: Arc<CallbackInstance>,
        ) -> Result<Arc<dyn PluginClient>, String> {
            self.opens.fetch_add(1, Ordering::SeqCst);
            let (open, cv) = &*self.release;
            let mut open = lock(open);
            while !*open {
                open = cv.wait(open).unwrap();
            }
            Ok(Arc::new(FakeClient))
        }
    }

    /// An apply abandoned while `open` blocks must not strand the
    /// plugin in `loading`; it completes and the plugin is active afterwards.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn abandoned_apply_still_completes() {
        let dir = std::env::temp_dir().join(format!("cpa-plugin-cancel-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(format!("p{}", platform::extension(platform::goos()))), b"").unwrap();
        let release = Arc::new((Mutex::new(false), std::sync::Condvar::new()));
        let loader = Arc::new(GateLoader {
            release: release.clone(),
            opens: AtomicUsize::new(0),
        });
        let host = Host::with_loader(loader.clone());
        let yaml = format!(
            "plugins:\n  enabled: true\n  dir: {}\n  configs:\n    p:\n      enabled: true\n",
            dir.display()
        );
        let cfg = Arc::new(Config::parse(&yaml).unwrap());
        let abandoned = tokio::time::timeout(Duration::from_millis(50), host.apply_config(cfg.clone())).await;
        assert!(abandoned.is_err(), "open is still blocked");
        *lock(&release.0) = true;
        release.1.notify_all();
        host.apply_config(cfg).await;
        assert!(host.plugin_registered("p"));
        assert_eq!(
            loader.opens.load(Ordering::SeqCst),
            1,
            "the abandoned load was reused, not repeated"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Registers with `caps`; while `gate` is closed, registration blocks.
    struct CapsClient {
        caps: &'static str,
        gate: Option<Arc<(Mutex<bool>, std::sync::Condvar)>>,
    }

    impl PluginClient for CapsClient {
        fn call(&self, method: &str, _: &[u8]) -> Result<Bytes, String> {
            if method != method::PLUGIN_REGISTER && method != method::PLUGIN_RECONFIGURE {
                return Ok(Bytes::from_static(br#"{"ok":true,"result":{}}"#));
            }
            if let Some((open, cv)) = self.gate.as_deref() {
                let mut open = lock(open);
                while !*open {
                    open = cv.wait(open).unwrap();
                }
            }
            Ok(Bytes::from(format!(
                r#"{{"ok":true,"result":{{"schema_version":6,"metadata":{{"Name":"n","Version":"1","Author":"a","GitHubRepository":"r"}},"capabilities":{{{}}}}}}}"#,
                self.caps
            )))
        }
        fn shutdown(&self) {}
    }

    /// Plugin `a` offers a request normalizer; `b` registers behind `gate`.
    struct HookLoader(Arc<(Mutex<bool>, std::sync::Condvar)>);

    impl Loader for HookLoader {
        fn open(
            &self,
            file: &PluginFile,
            _: Arc<dyn CallbackHandler>,
            _: Arc<CallbackInstance>,
        ) -> Result<Arc<dyn PluginClient>, String> {
            Ok(Arc::new(match file.id.as_str() {
                "a" => CapsClient {
                    caps: r#""request_normalizer":true"#,
                    gate: None,
                },
                _ => CapsClient {
                    caps: r#""usage_plugin":true"#,
                    gate: Some(self.0.clone()),
                },
            }))
        }
    }

    /// A hot reload that unfuses a translator plugin turns cpa-translate's hook flag
    /// on at once, while later plugins are still registering.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_hot_reload_unfusing_a_hook_plugin_activates_the_hooks_at_once() {
        let dir = std::env::temp_dir().join(format!("cpa-plugin-unfuse-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let ext = platform::extension(platform::goos());
        std::fs::write(dir.join(format!("a-v1{ext}")), b"").unwrap();
        std::fs::write(dir.join(format!("b{ext}")), b"").unwrap();
        let gate = Arc::new((Mutex::new(true), std::sync::Condvar::new()));
        let host = Host::with_loader(Arc::new(HookLoader(gate.clone())));
        let yaml = format!(
            "plugins:\n  enabled: true\n  dir: {}\n  configs:\n    a:\n      enabled: true\n    b:\n      enabled: true\n",
            dir.display()
        );
        let cfg = Arc::new(Config::parse(&yaml).unwrap());
        host.apply_config(cfg.clone()).await;
        cpa_translate::hooks::set_plugin_hooks(Some(host.translator_hooks()));
        assert!(cpa_translate::hooks::plugin_hooks_active());
        host.fuse("a", "RequestNormalizer.NormalizeRequest", "test");
        assert!(!cpa_translate::hooks::plugin_hooks_active());

        *lock(&gate.0) = false;
        // Reopens the gate even when an assertion fails, so the runtime can shut down.
        struct Reopen(Arc<(Mutex<bool>, std::sync::Condvar)>);
        impl Drop for Reopen {
            fn drop(&mut self) {
                *lock(&self.0.0) = true;
                self.0.1.notify_all();
            }
        }
        let reopen = Reopen(gate.clone());
        std::fs::write(dir.join(format!("a-v2{ext}")), b"").unwrap();
        let apply = tokio::spawn({
            let host = host.clone();
            async move { host.apply_config(cfg).await }
        });
        let unfused = async {
            while host.is_fused("a") {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        };
        tokio::time::timeout(Duration::from_secs(10), unfused)
            .await
            .expect("a reloads");
        assert!(!apply.is_finished(), "b's registration is still blocked");
        assert!(
            cpa_translate::hooks::plugin_hooks_active(),
            "a is eligible before b registers"
        );

        drop(reopen);
        apply.await.unwrap();
        assert!(cpa_translate::hooks::plugin_hooks_active());
        cpa_translate::hooks::set_plugin_hooks(None);
        assert!(
            !cpa_translate::hooks::plugin_hooks_active(),
            "an empty slot clears the flag"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
