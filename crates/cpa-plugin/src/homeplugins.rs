//! Home-managed plugin installs, deletes and their status reports (Go
//! internal/homeplugins and internal/home/plugin_status.go).
//!
//! Home either resolves the plugins to install (`plugin-sync`, with short-lived store
//! credentials) or, on Homes without plugin sync, leaves the node to install the
//! `store` manifests in its plugin configs. Every outcome becomes a [`SyncReport`] that
//! the node pushes to Home's `plugin-status` list.

use cpa_core::gostr::GoStr;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use futures_util::future::BoxFuture;
use serde_yaml_ng::Value;
use tokio_util::sync::CancellationToken;

use crate::config::{self, yaml_bool, yaml_int, yaml_string};
use crate::go_struct;
use crate::gojson::{self, GoTime, NonNil};
use crate::store::{
    AuthConfig, Client, Doer, InstallOptions, InstallResult, Manifest, PluginSyncItem, update_available,
};

const TASK_SYNC: &str = "plugin-sync";
const TASK_DELETE: &str = "plugin-delete";
const STATUS_OK: &str = "success";
const STATUS_ERROR: &str = "failed";
const PHASE_INSTALL: &str = "install";
const PHASE_LOAD: &str = "load";
const PHASE_DELETE: &str = "delete";
const INSTALLED: &str = "installed";
const SKIPPED: &str = "skipped";
const FAILED: &str = "failed";
const DELETED: &str = "deleted";
const MISSING: &str = "missing";
const LOADED: &str = "loaded";

/// Go `pluginStatusReportTimeout`.
const STATUS_REPORT_TIMEOUT: Duration = Duration::from_secs(10);

go_struct! {
    /// Go `homeplugins.Platform`.
    pub struct Platform("homeplugins.Platform") {
        "goos" => goos: String,
        "goarch" => goarch: String,
    }
}

go_struct! {
    /// Go `homeplugins.SyncReport`.
    pub struct SyncReport("homeplugins.SyncReport") {
        "schema_version" => schema_version: isize,
        "task_id" omitempty => task_id: u64,
        "task" => task: String,
        "node_id" omitempty => node_id: String,
        "status" => status: String,
        "phase" => phase: String,
        "ok" => ok: bool,
        "started_at" => started_at: GoTime,
        "finished_at" => finished_at: GoTime,
        "updated_at" => updated_at: GoTime,
        "platform" => platform: Platform,
        "plugins" => plugins: NonNil<Vec<PluginInstallStatus>>,
        "error" omitempty => error: String,
    }
}

go_struct! {
    /// Go `homeplugins.PluginInstallStatus`.
    pub struct PluginInstallStatus("homeplugins.PluginInstallStatus") {
        "id" => id: String,
        "version" omitempty => version: String,
        "release_tag" omitempty => release_tag: String,
        "repository" omitempty => repository: String,
        "install_type" omitempty => install_type: String,
        "install_status" => install_status: String,
        "load_status" omitempty => load_status: String,
        "path" omitempty => path: String,
        "skipped" omitempty => skipped: bool,
        "overwritten" omitempty => overwritten: bool,
        "error" omitempty => error: String,
    }
}

/// What the plugin host tells a sync (Go `PluginRuntime` and `PluginLoadInspector`).
pub trait PluginRuntime: Send + Sync {
    /// Loaded or still loading.
    fn plugin_busy(&self, id: &str) -> bool;
    /// Detaches and closes the plugin; `false` when nothing was unloaded. Cancelling
    /// `cancel` stops waiting for it (Go `UnloadPluginContext`).
    fn unload_plugin<'a>(&'a self, id: &'a str, cancel: &'a CancellationToken) -> BoxFuture<'a, bool>;
    /// Active in the current snapshot.
    fn plugin_registered(&self, id: &str) -> bool;
}

impl PluginRuntime for crate::Host {
    fn plugin_busy(&self, id: &str) -> bool {
        crate::Host::plugin_busy(self, id)
    }
    fn unload_plugin<'a>(&'a self, id: &'a str, cancel: &'a CancellationToken) -> BoxFuture<'a, bool> {
        Box::pin(crate::Host::unload_plugin_until(self, id, cancel))
    }
    fn plugin_registered(&self, id: &str) -> bool {
        crate::Host::plugin_registered(self, id)
    }
}

/// The parts of a config a sync reads (Go's `cfg.Plugins` and `cfg.ProxyURL`).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SyncConfig {
    pub enabled: bool,
    /// `plugins.dir` as configured.
    pub dir: String,
    /// Each `plugins.configs.<id>` subtree, by ID as written.
    pub configs: BTreeMap<String, Value>,
    pub auth_revision: i64,
    /// `plugins.store-auth`, normalized.
    pub store_auth: Vec<AuthConfig>,
    pub proxy_url: String,
}

impl SyncConfig {
    pub fn from_document(document: &Value) -> Self {
        let plugins = document.get("plugins");
        let field = |key: &str| plugins.and_then(|p| p.get(key));
        Self {
            enabled: field("enabled").and_then(yaml_bool).unwrap_or(false),
            dir: config::configured_dir(document),
            configs: match field("configs") {
                Some(Value::Mapping(map)) => map.iter().map(|(k, v)| (yaml_string(k), v.clone())).collect(),
                _ => BTreeMap::new(),
            },
            auth_revision: field("auth-revision").and_then(yaml_int).unwrap_or(0),
            store_auth: config::store_auth(document),
            proxy_url: document
                .get("requests")
                .and_then(|r| r.get("proxy-url"))
                .map(yaml_string)
                .unwrap_or_default()
                .trim()
                .to_owned(),
        }
    }

    /// The enabled plugin configs in ID order (Go `pluginConfigEnabled`).
    fn enabled_configs(&self) -> impl Iterator<Item = (&String, &Value)> {
        self.configs
            .iter()
            .filter(|(id, node)| config::item_config(id, node, None).enabled)
    }
}

/// Go `CurrentPlatform`.
pub fn current_platform() -> Platform {
    Platform {
        goos: crate::platform::goos().into(),
        goarch: crate::platform::goarch().into(),
    }
}

/// Go `NormalizePlatform`.
pub fn normalize_platform(platform: &Platform) -> Platform {
    let goos = match platform.goos.trim().go_lower().as_str() {
        "mac" | "macos" | "osx" => "darwin".to_owned(),
        other => other.to_owned(),
    };
    let goarch = match platform.goarch.trim().go_lower().as_str() {
        "x64" | "x86_64" => "amd64".to_owned(),
        "aarch64" => "arm64".to_owned(),
        other => other.to_owned(),
    };
    Platform { goos, goarch }
}

fn new_report(platform: &Platform) -> SyncReport {
    let now = GoTime::now_utc();
    SyncReport {
        schema_version: 1,
        task: TASK_SYNC.into(),
        status: STATUS_OK.into(),
        phase: PHASE_INSTALL.into(),
        ok: true,
        started_at: now,
        updated_at: now,
        platform: normalize_platform(platform),
        ..Default::default()
    }
}

/// Go `finishReport`.
fn finish(report: &mut SyncReport, error: Option<&str>) {
    let now = GoTime::now_utc();
    report.finished_at = now;
    report.updated_at = now;
    report.ok = error.is_none();
    match error {
        Some(e) => {
            report.status = STATUS_ERROR.into();
            report.error = e.to_owned();
        }
        None => {
            report.status = STATUS_OK.into();
            report.error.clear();
        }
    }
}

/// Go `errors.Join` of the collected messages.
fn joined(errors: &[String]) -> Option<String> {
    (!errors.is_empty()).then(|| errors.join("\n"))
}

/// Go `CompletedSyncReport`: a finished report for an outcome before any install.
pub fn completed_sync_report(platform: &Platform, error: Option<&str>) -> SyncReport {
    let mut report = new_report(platform);
    finish(&mut report, error);
    report
}

fn resolve_root(cfg: &SyncConfig) -> Result<PathBuf, String> {
    config::resolve_dir(&cfg.dir).map_err(|e| format!("home plugins: {e}"))
}

/// Go `newPluginStoreClient` and `newResolvedPluginStoreClient`: `http` already goes
/// through the config's proxy, which also scopes the shared GitHub cooldown.
fn store_client(cfg: &SyncConfig, http: &Arc<dyn Doer>) -> Client {
    let mut client = Client::new(http.clone());
    client.network_scope = cfg.proxy_url.clone();
    client
}

/// Go `storeManifestFromPluginConfig`: `None` without a `store` node.
fn store_manifest(id: &str, node: &Value) -> Result<Option<Manifest>, String> {
    let Some(store) = node.as_mapping().and_then(|m| m.get("store")).filter(|s| !s.is_null()) else {
        return Ok(None);
    };
    let mut manifest =
        Manifest::from_yaml(store).ok_or_else(|| format!("home plugins: decode store manifest for {id}"))?;
    if manifest.id.trim().is_empty() {
        manifest.id = id.trim().to_owned();
    }
    manifest
        .validate()
        .map_err(|e| format!("home plugins: invalid store manifest for {id}: {e}"))?;
    Ok(Some(manifest))
}

/// Go `pluginStatusFromManifest`.
fn status_from_manifest(manifest: &Manifest) -> PluginInstallStatus {
    PluginInstallStatus {
        id: manifest.id.trim().to_owned(),
        version: manifest.version.trim().to_owned(),
        release_tag: manifest.release_tag.trim().to_owned(),
        repository: manifest.repository.trim().to_owned(),
        install_type: manifest.install_type(),
        install_status: FAILED.into(),
        ..Default::default()
    }
}

fn record_install(status: &mut PluginInstallStatus, result: Result<InstallResult, String>) -> Option<String> {
    match result {
        Ok(result) => {
            status.path = result.path.trim().to_owned();
            status.skipped = result.skipped;
            status.overwritten = result.overwritten;
            status.install_status = if result.skipped { SKIPPED } else { INSTALLED }.into();
            None
        }
        Err(e) => {
            status.install_status = FAILED.into();
            status.error = e.clone();
            Some(e)
        }
    }
}

/// Go `installManifest`.
async fn install_manifest(
    client: &Client,
    manifest: &Manifest,
    root: &Path,
    platform: &Platform,
    runtime: Option<&Arc<dyn PluginRuntime>>,
) -> Result<InstallResult, String> {
    let id = manifest.id.trim().to_owned();
    if id.is_empty() {
        return Err("home plugins: manifest plugin id is empty".into());
    }
    let busy = runtime.cloned().map(|runtime| {
        let id = id.clone();
        Box::new(move || runtime.plugin_busy(&id)) as Box<dyn Fn() -> bool + Send + Sync>
    });
    let options = InstallOptions {
        plugins_dir: root.to_string_lossy().into_owned(),
        goos: platform.goos.clone(),
        goarch: platform.goarch.clone(),
        plugin_loaded: busy,
        before_write: None,
    };
    client
        .install_manifest(manifest, options)
        .await
        .map_err(|e| format!("home plugins: install {id}: {e}"))
}

/// Go `SyncWithReport`: installs each enabled plugin's `store` manifest for this
/// platform (Homes without plugin sync).
pub async fn sync_with_report(
    cfg: &SyncConfig,
    http: &Arc<dyn Doer>,
    runtime: Option<&Arc<dyn PluginRuntime>>,
) -> (SyncReport, Option<String>) {
    sync_platform_with_report(cfg, http, runtime, &current_platform()).await
}

/// Go `SyncPlatformWithReport`.
pub async fn sync_platform_with_report(
    cfg: &SyncConfig,
    http: &Arc<dyn Doer>,
    runtime: Option<&Arc<dyn PluginRuntime>>,
    platform: &Platform,
) -> (SyncReport, Option<String>) {
    if !cfg.enabled {
        return (new_report(platform), None);
    }
    let platform = normalize_platform(platform);
    let mut report = new_report(&platform);
    let fail = |mut report: SyncReport, e: String| {
        finish(&mut report, Some(&e));
        (report, Some(e))
    };
    if platform.goos.is_empty() {
        return fail(report, "home plugins: goos is required".into());
    }
    if platform.goarch.is_empty() {
        return fail(report, "home plugins: goarch is required".into());
    }
    let root = match resolve_root(cfg) {
        Ok(root) => root,
        Err(e) => return fail(report, e),
    };
    let mut client = store_client(cfg, http);
    client.auth = cfg.store_auth.clone();
    let mut errors = Vec::new();
    for (id, node) in cfg.enabled_configs() {
        let manifest = match store_manifest(id, node) {
            Ok(Some(manifest)) => manifest,
            Ok(None) => continue,
            Err(e) => {
                report.plugins.0.push(PluginInstallStatus {
                    id: id.trim().to_owned(),
                    install_status: FAILED.into(),
                    error: e.clone(),
                    ..Default::default()
                });
                errors.push(e);
                continue;
            }
        };
        let mut status = status_from_manifest(&manifest);
        let result = install_manifest(&client, &manifest, &root, &platform, runtime).await;
        errors.extend(record_install(&mut status, result));
        report.plugins.0.push(status);
    }
    let error = joined(&errors);
    finish(&mut report, error.as_deref());
    (report, error)
}

/// Go `SyncResolvedWithReport`: installs the plugins Home resolved, with the store
/// credentials it sent for them until `expires_at`. Installed plugins Home left out are
/// reported as skipped.
pub async fn sync_resolved_with_report(
    cfg: &SyncConfig,
    items: Vec<PluginSyncItem>,
    expires_at: GoTime,
    installed_versions: &BTreeMap<String, String>,
    http: &Arc<dyn Doer>,
    runtime: Option<&Arc<dyn PluginRuntime>>,
) -> (SyncReport, Option<String>) {
    let platform = normalize_platform(&current_platform());
    let mut report = new_report(&platform);
    if !cfg.enabled {
        finish(&mut report, None);
        return (report, None);
    }
    let root = match resolve_root(cfg) {
        Ok(root) => root,
        Err(e) => {
            finish(&mut report, Some(&e));
            return (report, Some(e));
        }
    };
    add_installed_version_statuses(&mut report, cfg, &root, installed_versions);
    let expires = expires_at.0.map(SystemTime::from);
    let mut errors = Vec::new();
    for item in items {
        if expires.is_none_or(|expires| SystemTime::now() >= expires) {
            errors.push("home plugins: plugin sync response expired".into());
            break;
        }
        let mut status = status_from_manifest(&item.manifest);
        let mut client = store_client(cfg, http);
        client.resolved_auth = item.auth;
        client.resolved_auth_expires_at = expires;
        let result = install_manifest(&client, &item.manifest, &root, &platform, runtime).await;
        drop(client);
        errors.extend(record_install(&mut status, result));
        upsert(&mut report, status);
    }
    let error = joined(&errors);
    finish(&mut report, error.as_deref());
    (report, error)
}

/// Go `addInstalledVersionStatuses`.
fn add_installed_version_statuses(
    report: &mut SyncReport,
    cfg: &SyncConfig,
    root: &Path,
    installed: &BTreeMap<String, String>,
) {
    if installed.is_empty() {
        return;
    }
    for (id, node) in cfg.enabled_configs() {
        let id = id.trim();
        let Some(version) = installed.get(id) else {
            continue;
        };
        let mut status = PluginInstallStatus {
            id: id.to_owned(),
            version: version.trim().to_owned(),
            install_status: SKIPPED.into(),
            skipped: true,
            ..Default::default()
        };
        if let Ok(files) = plugin_files(root, id)
            && let Some(file) = files.iter().find(|f| f.version.trim() == status.version)
        {
            status.path = file.path.to_string_lossy().trim().to_owned();
        }
        if let Ok(Some(manifest)) = store_manifest(id, node)
            && versions_equal(&status.version, &manifest.version)
        {
            status.release_tag = manifest.release_tag.trim().to_owned();
            status.repository = manifest.repository.trim().to_owned();
            status.install_type = manifest.install_type();
        }
        report.plugins.0.push(status);
    }
}

/// Go `pluginVersionsEqual`.
fn versions_equal(left: &str, right: &str) -> bool {
    let (left, right) = (left.trim(), right.trim());
    !left.is_empty() && !right.is_empty() && !update_available(left, right) && !update_available(right, left)
}

/// Go `upsertPluginInstallStatus`.
fn upsert(report: &mut SyncReport, status: PluginInstallStatus) {
    let id = status.id.trim().to_owned();
    match report.plugins.0.iter_mut().find(|p| p.id.trim() == id) {
        Some(existing) => *existing = status,
        None => report.plugins.0.push(status),
    }
}

/// Go `InstalledVersions`: the selected file's version of every configured plugin.
pub fn installed_versions(cfg: &SyncConfig) -> Result<BTreeMap<String, String>, String> {
    let root = resolve_root(cfg)?;
    let mut versions = BTreeMap::new();
    for id in cfg.configs.keys() {
        let files =
            plugin_files(&root, id).map_err(|e| format!("home plugins: discover installed plugin {id}: {e}"))?;
        if let Some(version) = files.first().map(|f| f.version.trim()).filter(|v| !v.is_empty()) {
            versions.insert(id.trim().to_owned(), version.to_owned());
        }
    }
    Ok(versions)
}

/// Go `MarkLoadResults`: moves the report to the load phase, failing it for every
/// installed plugin the host did not register. An earlier sync error is kept.
pub fn mark_load_results(report: &mut SyncReport, runtime: Option<&dyn PluginRuntime>) -> Option<String> {
    report.phase = PHASE_LOAD.into();
    let mut errors = Vec::new();
    let preserve = !report.ok && !report.error.trim().is_empty();
    if preserve {
        errors.push(report.error.clone());
    }
    for status in &mut report.plugins.0 {
        if status.install_status == FAILED {
            if status.load_status.is_empty() {
                status.load_status = SKIPPED.into();
            }
            if !preserve {
                errors.push(match status.error.trim() {
                    "" => format!("home plugins: plugin {} install failed", status.id),
                    _ => status.error.clone(),
                });
            }
            continue;
        }
        if runtime.is_some_and(|r| r.plugin_registered(&status.id)) {
            status.load_status = LOADED.into();
            continue;
        }
        status.load_status = FAILED.into();
        let error = format!("home plugins: plugin {} installed but not loaded", status.id);
        if status.error.trim().is_empty() {
            status.error = error.clone();
        }
        errors.push(error);
    }
    let error = joined(&errors);
    finish(report, error.as_deref());
    error
}

/// Go `DeleteWithReport`: removes every file of the plugin for this platform,
/// unloading it first when it is busy.
pub async fn delete_with_report(
    cancel: &CancellationToken,
    cfg: Option<&SyncConfig>,
    runtime: Option<&Arc<dyn PluginRuntime>>,
    task_id: u64,
    plugin_id: &str,
) -> SyncReport {
    let mut report = new_report(&current_platform());
    report.task_id = task_id;
    report.task = TASK_DELETE.into();
    report.phase = PHASE_DELETE.into();
    let mut status = PluginInstallStatus {
        id: plugin_id.trim().to_owned(),
        ..Default::default()
    };
    let outcome = match cfg {
        _ if cancel.is_cancelled() => Err(CANCELED.to_owned()),
        None => Err("home plugins: config is nil".to_owned()),
        Some(cfg) => match resolve_root(cfg) {
            Err(e) => Err(e),
            Ok(_) if cancel.is_cancelled() => Err(CANCELED.to_owned()),
            Ok(root) => {
                let (path, deleted, result) = delete_artifact(cancel, &root, &status.id, runtime).await;
                status.path = path.trim().to_owned();
                result.map(|()| deleted)
            }
        },
    };
    let error = match outcome {
        Ok(deleted) => {
            status.install_status = if deleted { DELETED } else { MISSING }.into();
            None
        }
        Err(e) => {
            status.install_status = FAILED.into();
            status.error = e.clone();
            Some(e)
        }
    };
    report.plugins.0.push(status);
    finish(&mut report, error.as_deref());
    report
}

/// Go's `context.Canceled` text.
const CANCELED: &str = "context canceled";

/// Go `deletePluginArtifact`: the representative path, whether a file went, and the
/// error.
async fn delete_artifact(
    cancel: &CancellationToken,
    root: &Path,
    id: &str,
    runtime: Option<&Arc<dyn PluginRuntime>>,
) -> (String, bool, Result<(), String>) {
    if !valid_file_id(id) {
        return (
            String::new(),
            false,
            Err(format!("invalid plugin id {}", cpa_common::gostr::quote(id))),
        );
    }
    let paths: Vec<String> = match plugin_files(root, id) {
        Ok(files) => files.iter().map(|f| f.path.to_string_lossy().into_owned()).collect(),
        Err(e) => return (String::new(), false, Err(e.to_string())),
    };
    if cancel.is_cancelled() {
        return (String::new(), false, Err(CANCELED.into()));
    }
    let Some(first) = paths.first().cloned() else {
        return (String::new(), false, Ok(()));
    };
    if let Some(runtime) = runtime
        && runtime.plugin_busy(id)
    {
        if cancel.is_cancelled() {
            return (first, false, Err(CANCELED.into()));
        }
        if !runtime.unload_plugin(id, cancel).await && runtime.plugin_busy(id) {
            return (first, false, Err(crate::store::LOADED_PLUGIN_LOCKED.into()));
        }
    }
    let mut deleted = false;
    for path in &paths {
        if cancel.is_cancelled() {
            return (first, deleted, Err(CANCELED.into()));
        }
        match std::fs::remove_file(path) {
            Ok(()) => deleted = true,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return (first, deleted, Err(e.to_string())),
        }
    }
    (first, deleted, Ok(()))
}

/// Go `pluginFileInfo`.
struct PluginFile {
    path: PathBuf,
    version: String,
}

/// Go `pluginFileInfos`: the plugin's files for this platform, the preferred one first.
fn plugin_files(root: &Path, id: &str) -> std::io::Result<Vec<PluginFile>> {
    let root = if root.as_os_str().is_empty() {
        Path::new("plugins")
    } else {
        root
    };
    let id = id.trim();
    let platform = current_platform();
    let extension = crate::platform::extension(&platform.goos);
    let mut candidates = Vec::new();
    for dir in crate::platform::candidate_dirs(root, &platform.goos, &platform.goarch) {
        let entries = match std::fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => return Err(e),
        };
        let mut names = Vec::new();
        for entry in entries {
            let entry = entry?;
            // Go's DirEntry type is lstat-based: symlinks are not regular files.
            if entry.file_type()?.is_file() && entry.file_name().to_string_lossy().go_lower().ends_with(extension) {
                names.push(dir.join(entry.file_name()));
            }
        }
        names.sort_by(|a, b| a.as_os_str().as_encoded_bytes().cmp(b.as_os_str().as_encoded_bytes()));
        for path in names {
            if let Some((file_id, version)) = split_file_name(&path, extension)
                && file_id == id
            {
                candidates.push(PluginFile { path, version });
            }
        }
    }
    let best = (1..candidates.len()).fold(0, |best, i| {
        if preferred(&candidates[i], &candidates[best]) {
            i
        } else {
            best
        }
    });
    if best != 0 {
        let file = candidates.remove(best);
        candidates.insert(0, file);
    }
    Ok(candidates)
}

/// Go `pluginFileFromPath`: `(id, version)` of `<id>[-v<version>]<extension>`.
fn split_file_name(path: &Path, extension: &str) -> Option<(String, String)> {
    let base = path.file_name()?.to_string_lossy().into_owned();
    if !base.go_lower().ends_with(extension) {
        return None;
    }
    let name = &base[..base.len() - extension.len()];
    let (mut id, mut version) = (name, "");
    if let Some(i) = name.rfind("-v").filter(|i| *i > 0) {
        let (candidate_id, candidate_version) = (&name[..i], &name[i + 2..]);
        if valid_file_id(candidate_id) && valid_file_version(candidate_version) {
            (id, version) = (candidate_id, candidate_version);
        }
    }
    valid_file_id(id).then(|| (id.to_owned(), version.to_owned()))
}

/// Go `pluginFilePreferred`.
fn preferred(candidate: &PluginFile, current: &PluginFile) -> bool {
    if candidate.version.is_empty() {
        return false;
    }
    current.version.is_empty() || update_available(&current.version, &candidate.version)
}

/// Go homeplugins `validPluginFileID`.
fn valid_file_id(id: &str) -> bool {
    let id = id.trim();
    !id.is_empty()
        && id != "."
        && id != ".."
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
}

/// Go homeplugins `validPluginFileVersion`.
fn valid_file_version(version: &str) -> bool {
    version.trim().as_bytes().first().is_some_and(u8::is_ascii_digit)
}

/// Go `home.ReportPluginStatus`: stamps the node and time on `report` and pushes it with
/// `push` (Go `RPushPluginStatus`) within ten seconds.
pub async fn report_plugin_status<F, Fut>(push: F, node_id: &str, mut report: SyncReport) -> Result<(), String>
where
    F: FnOnce(Vec<u8>) -> Fut,
    Fut: std::future::Future<Output = Result<(), String>>,
{
    let node_id = node_id.trim();
    if node_id.is_empty() {
        return Err("home plugin status node id is empty".into());
    }
    report.node_id = node_id.to_owned();
    report.updated_at = GoTime::now_utc();
    let payload = gojson::to_vec(&report);
    tokio::time::timeout(STATUS_REPORT_TIMEOUT, push(payload))
        .await
        .unwrap_or_else(|_| Err("context deadline exceeded".into()))
}

#[cfg(test)]
mod tests {
    //! Go internal/homeplugins/sync_test.go, network_scope_test.go and
    //! internal/home/plugin_status_test.go.

    use std::sync::Mutex;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    use bytes::Bytes;
    use futures_util::StreamExt as _;
    use sha2::{Digest as _, Sha256};

    use super::*;
    use crate::store::{
        Artifact, DoerResponse, Headers, INSTALL_TYPE_DIRECT, INSTALL_TYPE_GITHUB_RELEASE, InstallPlan, Plugin,
        REQUEST_KIND_ARTIFACT, ResolvedAuthConfig, Secret, StoreError,
    };

    /// Go `mapHTTPDoer`: fixed bodies by exact URL, 404 otherwise. `bearer` makes every
    /// route answer 401 unless the request carries that bearer token.
    #[derive(Default)]
    struct MapDoer {
        routes: BTreeMap<String, Vec<u8>>,
        bearer: Option<String>,
        authenticated: AtomicBool,
        calls: AtomicUsize,
        status: Option<u16>,
    }

    impl Doer for MapDoer {
        fn get(&self, url: &str, headers: &Headers) -> BoxFuture<'_, Result<DoerResponse, String>> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let authorized = match &self.bearer {
                None => true,
                Some(token) => headers
                    .get("Authorization")
                    .is_some_and(|v| v.iter().any(|v| *v == format!("Bearer {token}"))),
            };
            if self.bearer.is_some() && authorized {
                self.authenticated.store(true, Ordering::SeqCst);
            }
            let (status, body) = match (self.status, self.routes.get(url)) {
                (Some(status), _) => (status, b"proxy".to_vec()),
                _ if !authorized => (401, b"unauthorized".to_vec()),
                (None, Some(body)) => (200, body.clone()),
                (None, None) => (404, b"not found".to_vec()),
            };
            Box::pin(async move {
                Ok(DoerResponse {
                    status,
                    headers: Headers::new(),
                    body: futures_util::stream::once(async move { Ok(Bytes::from(body)) }).boxed(),
                })
            })
        }
    }

    #[derive(Default)]
    struct FakeRuntime {
        busy: AtomicBool,
        unloaded: Mutex<Vec<String>>,
        /// The cancellation each unload received.
        unload_cancels: Mutex<Vec<CancellationToken>>,
        registered: Vec<&'static str>,
    }

    impl PluginRuntime for FakeRuntime {
        fn plugin_busy(&self, _id: &str) -> bool {
            self.busy.load(Ordering::SeqCst)
        }
        fn unload_plugin<'a>(&'a self, id: &'a str, cancel: &'a CancellationToken) -> BoxFuture<'a, bool> {
            self.unloaded.lock().unwrap().push(id.to_owned());
            self.unload_cancels.lock().unwrap().push(cancel.clone());
            self.busy.store(false, Ordering::SeqCst);
            Box::pin(async { true })
        }
        fn plugin_registered(&self, id: &str) -> bool {
            self.registered.contains(&id)
        }
    }

    fn busy_runtime() -> Arc<FakeRuntime> {
        let runtime = FakeRuntime::default();
        runtime.busy.store(true, Ordering::SeqCst);
        Arc::new(runtime)
    }

    fn dyn_runtime(runtime: &Arc<FakeRuntime>) -> Arc<dyn PluginRuntime> {
        runtime.clone()
    }

    /// A stored (uncompressed) zip archive, as Go's `zip.Writer` with `Create` would
    /// hold the same files.
    fn make_zip(files: &[(&str, &str)]) -> Vec<u8> {
        let (mut out, mut central) = (Vec::new(), Vec::new());
        for (name, content) in files {
            let mut crc = flate2::Crc::new();
            crc.update(content.as_bytes());
            let offset = out.len() as u32;
            let header = |sig: u32, central: bool| {
                let mut h = sig.to_le_bytes().to_vec();
                if central {
                    h.extend(20u16.to_le_bytes()); // made by: FAT, 2.0
                }
                h.extend(20u16.to_le_bytes()); // version needed
                h.extend(0u16.to_le_bytes()); // flags
                h.extend(0u16.to_le_bytes()); // stored
                h.extend(0u32.to_le_bytes()); // time and date
                h.extend(crc.sum().to_le_bytes());
                h.extend((content.len() as u32).to_le_bytes());
                h.extend((content.len() as u32).to_le_bytes());
                h.extend((name.len() as u16).to_le_bytes());
                h.extend(0u16.to_le_bytes()); // extra
                if central {
                    h.extend([0u8; 6]); // comment, disk, internal attributes
                    h.extend(0u32.to_le_bytes()); // external attributes
                    h.extend(offset.to_le_bytes());
                }
                h.extend(name.as_bytes());
                h
            };
            out.extend(header(0x0403_4b50, false));
            out.extend(content.as_bytes());
            central.extend(header(0x0201_4b50, true));
        }
        let (start, size) = (out.len() as u32, central.len() as u32);
        out.extend(central);
        out.extend(0x0605_4b50u32.to_le_bytes());
        out.extend([0u8; 4]);
        out.extend((files.len() as u16).to_le_bytes());
        out.extend((files.len() as u16).to_le_bytes());
        out.extend(size.to_le_bytes());
        out.extend(start.to_le_bytes());
        out.extend(0u16.to_le_bytes());
        out
    }

    fn hex_sha256(data: &[u8]) -> String {
        Sha256::digest(data).iter().map(|b| format!("{b:02x}")).collect()
    }

    /// The GitHub release routes for sample v0.2.0 on windows/amd64.
    fn release_doer() -> Arc<dyn Doer> {
        let archive = make_zip(&[("sample.dll", "library-data")]);
        let name = "sample_0.2.0_windows_amd64.zip";
        let mut routes = BTreeMap::new();
        routes.insert(
            "https://api.github.com/repos/owner/sample-plugin/releases/tags/v0.2.0".to_owned(),
            format!(
                r#"{{"tag_name": "v0.2.0", "assets": [
                    {{"name": "{name}", "browser_download_url": "https://downloads.example/{name}"}},
                    {{"name": "checksums.txt", "browser_download_url": "https://downloads.example/checksums.txt"}}]}}"#
            )
            .into_bytes(),
        );
        routes.insert(
            "https://downloads.example/checksums.txt".to_owned(),
            format!("{}  {name}\n", hex_sha256(&archive)).into_bytes(),
        );
        routes.insert(format!("https://downloads.example/{name}"), archive);
        Arc::new(MapDoer {
            routes,
            ..Default::default()
        })
    }

    fn config_yaml(root: &Path, configs: &str) -> SyncConfig {
        let text = format!(
            "plugins:\n  enabled: true\n  dir: {}\n  configs:\n{configs}",
            root.display()
        );
        SyncConfig::from_document(&serde_yaml_ng::from_str(&text).unwrap())
    }

    /// Go `syncTestConfig`.
    fn sync_test_config(root: &Path) -> SyncConfig {
        config_yaml(
            root,
            "    sample:
      enabled: true
      store:
        id: sample
        name: Sample
        description: Adds sample support.
        author: owner
        version: 0.2.0
        release-tag: v0.2.0
        repository: https://github.com/owner/sample-plugin
",
        )
    }

    /// Go `pluginTestPath`.
    fn plugin_test_path(root: &Path, goos: &str, goarch: &str, id: &str, version: &str) -> PathBuf {
        let mut name = id.to_owned();
        if !version.is_empty() {
            name += &format!("-v{version}");
        }
        root.join(goos)
            .join(goarch)
            .join(name + crate::platform::extension(goos))
    }

    fn write_plugin(path: &Path) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, "library-data").unwrap();
    }

    fn windows() -> Platform {
        Platform {
            goos: "windows".into(),
            goarch: "amd64".into(),
        }
    }

    fn linux() -> Platform {
        Platform {
            goos: "linux".into(),
            goarch: "amd64".into(),
        }
    }

    fn host_platform() -> (String, String) {
        let p = current_platform();
        (p.goos, p.goarch)
    }

    fn in_a_minute() -> GoTime {
        GoTime(Some(
            (chrono::Utc::now() + chrono::Duration::seconds(60)).fixed_offset(),
        ))
    }

    #[tokio::test]
    async fn sync_platform_installs_manifest_artifact() {
        let root = tempdir();
        let (_, error) = sync_platform_with_report(&sync_test_config(&root), &release_doer(), None, &windows()).await;
        assert_eq!(error, None);
        let target = plugin_test_path(&root, "windows", "amd64", "sample", "0.2.0");
        assert_eq!(std::fs::read_to_string(target).unwrap(), "library-data");
    }

    #[tokio::test]
    async fn sync_resolved_with_report_uses_temporary_auth_and_clears_it() {
        let root = tempdir();
        let (goos, goarch) = host_platform();
        let library = format!("sample{}", crate::platform::extension(&goos));
        let archive = make_zip(&[(&library, "library-data")]);
        let url = "https://downloads.example/private/sample.zip";
        let doer = Arc::new(MapDoer {
            routes: BTreeMap::from([(url.to_owned(), archive.clone())]),
            bearer: Some("temporary-token".into()),
            ..Default::default()
        });
        let items = vec![PluginSyncItem {
            manifest: Manifest {
                schema_version: 2,
                id: "sample".into(),
                version: "1.0.0".into(),
                install: InstallPlan {
                    install_type: INSTALL_TYPE_DIRECT.into(),
                    artifacts: vec![Artifact {
                        goos: goos.clone(),
                        goarch: goarch.clone(),
                        url: url.into(),
                        sha256: hex_sha256(&archive),
                        size: archive.len() as i64,
                    }],
                },
                ..Default::default()
            },
            auth: vec![ResolvedAuthConfig {
                matches: "https://downloads.example/private/".into(),
                apply_to: vec![REQUEST_KIND_ARTIFACT.into()],
                auth_type: "bearer".into(),
                token: Secret(b"temporary-token".to_vec()),
                ..Default::default()
            }],
        }];
        let cfg = config_yaml(&root, "    sample:\n      enabled: true\n");
        let installed = BTreeMap::from([("sample".to_owned(), "0.9.0".to_owned())]);
        let http: Arc<dyn Doer> = doer.clone();
        // The items move into the sync, which drops (and so zeroes) their credentials.
        let (report, error) = sync_resolved_with_report(&cfg, items, in_a_minute(), &installed, &http, None).await;
        assert_eq!(error, None);
        assert!(doer.authenticated.load(Ordering::SeqCst));
        assert!(report.ok);
        assert_eq!(report.plugins.0.len(), 1);
        assert_eq!(report.plugins.0[0].version, "1.0.0");
        let target = plugin_test_path(&root, &goos, &goarch, "sample", "1.0.0");
        assert_eq!(std::fs::read_to_string(target).unwrap(), "library-data");
    }

    #[tokio::test]
    async fn sync_resolved_with_report_includes_unchanged_installed_plugins() {
        let root = tempdir();
        let (goos, goarch) = host_platform();
        let target = plugin_test_path(&root, &goos, &goarch, "sample", "1.0.0");
        write_plugin(&target);
        let cfg = config_yaml(
            &root,
            "    sample:
      enabled: true
      store:
        id: sample
        name: Sample
        description: Adds sample support.
        author: owner
        version: 1.0.0
        release-tag: v1.0.0
        repository: https://github.com/owner/sample-plugin
",
        );
        let installed = BTreeMap::from([("sample".to_owned(), "1.0.0".to_owned())]);
        let http: Arc<dyn Doer> = Arc::new(MapDoer::default());
        let (mut report, error) =
            sync_resolved_with_report(&cfg, Vec::new(), in_a_minute(), &installed, &http, None).await;
        assert_eq!(error, None);
        assert_eq!(report.plugins.0.len(), 1);
        let status = &report.plugins.0[0];
        assert_eq!(
            (status.id.as_str(), status.install_status.as_str()),
            ("sample", SKIPPED)
        );
        assert_eq!(status.path, target.to_string_lossy());
        assert_eq!(status.release_tag, "v1.0.0");
        assert_eq!(status.repository, "https://github.com/owner/sample-plugin");
        assert_eq!(status.install_type, INSTALL_TYPE_GITHUB_RELEASE);
        assert!(mark_load_results(&mut report, Some(&FakeRuntime::default())).is_some());
        assert_eq!(report.plugins.0[0].load_status, FAILED);
    }

    #[tokio::test]
    async fn sync_resolved_with_report_does_not_mix_installed_and_configured_metadata() {
        let root = tempdir();
        let (goos, goarch) = host_platform();
        let target = plugin_test_path(&root, &goos, &goarch, "sample", "1.0.0");
        write_plugin(&target);
        let cfg = config_yaml(
            &root,
            "    sample:
      enabled: true
      store:
        id: sample
        name: Sample
        description: Adds sample support.
        author: owner
        version: 2.0.0
        release-tag: v2.0.0
        repository: https://github.com/owner/sample-plugin-v2
",
        );
        let installed = BTreeMap::from([("sample".to_owned(), "1.0.0".to_owned())]);
        let http: Arc<dyn Doer> = Arc::new(MapDoer::default());
        let (report, error) = sync_resolved_with_report(&cfg, Vec::new(), in_a_minute(), &installed, &http, None).await;
        assert_eq!(error, None);
        assert_eq!(report.plugins.0.len(), 1);
        let status = &report.plugins.0[0];
        assert_eq!(
            (status.version.as_str(), status.path.as_str()),
            ("1.0.0", &*target.to_string_lossy())
        );
        assert_eq!(
            (
                status.release_tag.as_str(),
                status.repository.as_str(),
                status.install_type.as_str()
            ),
            ("", "", "")
        );
    }

    #[test]
    fn installed_versions_uses_plugin_files_on_disk() {
        let root = tempdir();
        let (goos, goarch) = host_platform();
        write_plugin(&plugin_test_path(&root, &goos, &goarch, "sample", "2.3.4"));
        let cfg = config_yaml(&root, "    sample: {}\n");
        let versions = installed_versions(&cfg).unwrap();
        assert_eq!(versions.get("sample").map(String::as_str), Some("2.3.4"));
    }

    #[tokio::test]
    async fn sync_platform_with_report_records_successful_install() {
        let root = tempdir();
        let (report, error) =
            sync_platform_with_report(&sync_test_config(&root), &release_doer(), None, &windows()).await;
        assert_eq!(error, None);
        assert!(report.ok);
        assert_eq!(
            (report.status.as_str(), report.phase.as_str()),
            (STATUS_OK, PHASE_INSTALL)
        );
        assert_eq!(report.plugins.0.len(), 1);
        let plugin = &report.plugins.0[0];
        assert_eq!(
            (
                plugin.id.as_str(),
                plugin.install_status.as_str(),
                plugin.version.as_str()
            ),
            ("sample", INSTALLED, "0.2.0")
        );
        let want = plugin_test_path(&root, "windows", "amd64", "sample", "0.2.0");
        assert_eq!(plugin.path, want.to_string_lossy());
    }

    #[tokio::test]
    async fn sync_platform_with_report_records_skipped_identical_artifact() {
        let root = tempdir();
        let target = plugin_test_path(&root, "windows", "amd64", "sample", "0.2.0");
        write_plugin(&target);
        let (report, error) =
            sync_platform_with_report(&sync_test_config(&root), &release_doer(), None, &windows()).await;
        assert_eq!(error, None);
        assert!(report.ok);
        assert_eq!(report.plugins.0.len(), 1);
        let plugin = &report.plugins.0[0];
        assert_eq!(
            (plugin.id.as_str(), plugin.install_status.as_str()),
            ("sample", SKIPPED)
        );
        assert!(plugin.skipped);
        assert_eq!(plugin.path, target.to_string_lossy());
    }

    #[tokio::test]
    async fn sync_platform_skips_identical_busy_plugin() {
        let root = tempdir();
        let target = plugin_test_path(&root, "windows", "amd64", "sample", "0.2.0");
        write_plugin(&target);
        let runtime = busy_runtime();
        let (_, error) = sync_platform_with_report(
            &sync_test_config(&root),
            &release_doer(),
            Some(&dyn_runtime(&runtime)),
            &windows(),
        )
        .await;
        assert_eq!(error, None);
        assert!(runtime.unloaded.lock().unwrap().is_empty());
        assert_eq!(std::fs::read_to_string(target).unwrap(), "library-data");
    }

    #[tokio::test]
    async fn sync_platform_skips_config_without_manifest() {
        let root = tempdir();
        let cfg = config_yaml(&root, "    sample:\n      enabled: true\n");
        let http: Arc<dyn Doer> = Arc::new(MapDoer::default());
        let (report, error) = sync_platform_with_report(&cfg, &http, None, &linux()).await;
        assert_eq!(error, None);
        assert!(report.plugins.0.is_empty());
    }

    #[tokio::test]
    async fn sync_platform_rejects_invalid_manifest() {
        let root = tempdir();
        let cfg = config_yaml(
            &root,
            "    sample:\n      enabled: true\n      store:\n        id: sample\n",
        );
        let http: Arc<dyn Doer> = Arc::new(MapDoer::default());
        assert!(sync_platform_with_report(&cfg, &http, None, &linux()).await.1.is_some());
    }

    #[tokio::test]
    async fn sync_platform_with_report_records_invalid_manifest() {
        let root = tempdir();
        let cfg = config_yaml(
            &root,
            "    sample:\n      enabled: true\n      store:\n        id: sample\n",
        );
        let http: Arc<dyn Doer> = Arc::new(MapDoer::default());
        let (report, error) = sync_platform_with_report(&cfg, &http, None, &linux()).await;
        assert!(error.is_some());
        assert!(!report.ok);
        assert_eq!(report.status, STATUS_ERROR);
        assert_eq!(report.plugins.0.len(), 1);
        let plugin = &report.plugins.0[0];
        assert_eq!((plugin.id.as_str(), plugin.install_status.as_str()), ("sample", FAILED));
        assert!(plugin.error.contains("invalid store manifest"), "{}", plugin.error);
    }

    fn installed_report(id: &str, install_status: &str, error: &str) -> SyncReport {
        SyncReport {
            status: STATUS_OK.into(),
            ok: true,
            phase: PHASE_INSTALL.into(),
            plugins: NonNil(vec![PluginInstallStatus {
                id: id.into(),
                install_status: install_status.into(),
                error: error.into(),
                ..Default::default()
            }]),
            ..Default::default()
        }
    }

    #[test]
    fn mark_load_results_fails_when_installed_plugin_did_not_load() {
        let mut report = installed_report("sample", INSTALLED, "");
        assert!(mark_load_results(&mut report, Some(&FakeRuntime::default())).is_some());
        assert!(!report.ok);
        assert_eq!(
            (report.status.as_str(), report.phase.as_str()),
            (STATUS_ERROR, PHASE_LOAD)
        );
        assert_eq!(report.plugins.0[0].load_status, FAILED);
        assert!(report.plugins.0[0].error.contains("installed but not loaded"));
    }

    #[test]
    fn mark_load_results_preserves_install_failure() {
        let mut report = installed_report("sample", FAILED, "install boom");
        report.status = STATUS_ERROR.into();
        report.ok = false;
        let runtime = FakeRuntime {
            registered: vec!["sample"],
            ..Default::default()
        };
        assert!(mark_load_results(&mut report, Some(&runtime)).is_some());
        assert!(!report.ok);
        assert_eq!(report.status, STATUS_ERROR);
        assert_eq!(report.plugins.0[0].load_status, SKIPPED);
    }

    #[test]
    fn mark_load_results_preserves_global_sync_failure() {
        let mut report = new_report(&linux());
        report.plugins.0.push(PluginInstallStatus {
            id: "installed".into(),
            install_status: INSTALLED.into(),
            ..Default::default()
        });
        finish(&mut report, Some("home plugins: plugin sync response expired"));
        let runtime = FakeRuntime {
            registered: vec!["installed"],
            ..Default::default()
        };
        let error = mark_load_results(&mut report, Some(&runtime)).unwrap();
        assert!(error.contains("plugin sync response expired"));
        assert!(!report.ok);
        assert_eq!(
            (report.status.as_str(), report.phase.as_str()),
            (STATUS_ERROR, PHASE_LOAD)
        );
        assert!(report.error.contains("plugin sync response expired"));
        assert_eq!(report.plugins.0[0].load_status, LOADED);
    }

    #[test]
    fn completed_sync_report_both_outcomes() {
        // Go TestCompletedSyncReport.
        let ok = completed_sync_report(&linux(), None);
        assert!(ok.ok && ok.task == TASK_SYNC && ok.finished_at.0.is_some());
        let failure = "home plugins: inspect installed plugins: access denied";
        let failed = completed_sync_report(&linux(), Some(failure));
        assert!(!failed.ok && failed.task == TASK_SYNC && failed.finished_at.0.is_some());
        assert_eq!((failed.status.as_str(), failed.error.as_str()), (STATUS_ERROR, failure));
    }

    #[tokio::test]
    async fn delete_with_report_rejects_unresolved_plugins_dir() {
        let cfg = SyncConfig {
            dir: "~/.cli-proxy-api/plugins".into(),
            ..Default::default()
        };
        let home = std::env::var_os("HOME");
        // SAFETY: nothing else in this crate's tests reads $HOME (nextest also runs
        // each test in its own process).
        unsafe { std::env::remove_var("HOME") };
        let report = delete_with_report(&CancellationToken::new(), Some(&cfg), None, 41, "sample").await;
        if let Some(home) = home {
            unsafe { std::env::set_var("HOME", home) };
        }
        assert!(!report.ok);
        assert_eq!(report.status, STATUS_ERROR);
        assert_eq!(report.plugins.0.len(), 1);
        assert_eq!(report.plugins.0[0].install_status, FAILED);
        assert!(report.plugins.0[0].error.contains("resolve plugins directory"));
    }

    #[tokio::test]
    async fn delete_with_report_removes_current_platform_plugin() {
        let root = tempdir();
        let (goos, goarch) = host_platform();
        let target = plugin_test_path(&root, &goos, &goarch, "sample", "");
        write_plugin(&target);
        let runtime = busy_runtime();
        let report = delete_with_report(
            &CancellationToken::new(),
            Some(&sync_test_config(&root)),
            Some(&dyn_runtime(&runtime)),
            42,
            "sample",
        )
        .await;
        assert!(report.ok);
        assert_eq!(report.task_id, 42);
        assert_eq!(
            (report.task.as_str(), report.phase.as_str()),
            (TASK_DELETE, PHASE_DELETE)
        );
        assert_eq!(*runtime.unloaded.lock().unwrap(), ["sample"]);
        assert_eq!(report.plugins.0.len(), 1);
        assert_eq!(report.plugins.0[0].install_status, DELETED);
        assert_eq!(report.plugins.0[0].path, target.to_string_lossy());
        assert!(!target.exists());
    }

    #[tokio::test]
    async fn delete_with_report_removes_all_current_platform_plugin_versions() {
        let root = tempdir();
        let (goos, goarch) = host_platform();
        let older = plugin_test_path(&root, &goos, &goarch, "sample", "0.2.0");
        let newer = plugin_test_path(&root, &goos, &goarch, "sample", "0.3.0");
        let other = plugin_test_path(&root, &goos, &goarch, "other", "0.3.0");
        for path in [&older, &newer, &other] {
            write_plugin(path);
        }
        let runtime = busy_runtime();
        let report = delete_with_report(
            &CancellationToken::new(),
            Some(&sync_test_config(&root)),
            Some(&dyn_runtime(&runtime)),
            43,
            "sample",
        )
        .await;
        assert!(report.ok);
        assert_eq!(*runtime.unloaded.lock().unwrap(), ["sample"]);
        assert_eq!(report.plugins.0[0].install_status, DELETED);
        // The newest version is the representative path.
        assert_eq!(report.plugins.0[0].path, newer.to_string_lossy());
        assert!(!older.exists() && !newer.exists());
        assert!(other.exists());
    }

    #[tokio::test]
    async fn delete_with_report_stops_before_unload_when_context_canceled() {
        let root = tempdir();
        let (goos, goarch) = host_platform();
        let path = plugin_test_path(&root, &goos, &goarch, "sample", "1.0.0");
        write_plugin(&path);
        let runtime = busy_runtime();
        let cancel = CancellationToken::new();
        cancel.cancel();
        let report = delete_with_report(
            &cancel,
            Some(&sync_test_config(&root)),
            Some(&dyn_runtime(&runtime)),
            44,
            "sample",
        )
        .await;
        assert!(!report.ok);
        assert!(report.error.contains(CANCELED));
        assert!(runtime.unloaded.lock().unwrap().is_empty());
        assert!(path.exists());
    }

    #[tokio::test]
    async fn delete_with_report_uses_contextual_unload() {
        // Go passes the task's context to the unload: cancelling the delete cancels the
        // unload it started. The host side is `a_cancelled_home_unload_releases_the_host`.
        let root = tempdir();
        let (goos, goarch) = host_platform();
        write_plugin(&plugin_test_path(&root, &goos, &goarch, "sample", "1.0.0"));
        let runtime = busy_runtime();
        let cancel = CancellationToken::new();
        let report = delete_with_report(
            &cancel,
            Some(&sync_test_config(&root)),
            Some(&dyn_runtime(&runtime)),
            45,
            "sample",
        )
        .await;
        assert!(report.ok, "{report:?}");
        assert_eq!(*runtime.unloaded.lock().unwrap(), ["sample"]);
        let received = runtime.unload_cancels.lock().unwrap()[0].clone();
        assert!(!received.is_cancelled());
        cancel.cancel();
        assert!(
            received.is_cancelled(),
            "the unload did not get the delete's cancellation"
        );
    }

    #[tokio::test]
    async fn delete_with_report_missing_plugin_is_success() {
        let root = tempdir();
        let report = delete_with_report(
            &CancellationToken::new(),
            Some(&sync_test_config(&root)),
            None,
            7,
            "missing",
        )
        .await;
        assert!(report.ok);
        assert_eq!(report.status, STATUS_OK);
        assert_eq!(report.plugins.0.len(), 1);
        assert_eq!(report.plugins.0[0].install_status, MISSING);
    }

    #[tokio::test]
    async fn plugin_store_clients_share_proxy_cooldown() {
        // Go network_scope_test.go: Home's store clients key the shared GitHub cooldown
        // by the trimmed proxy URL, so a cooldown seeded through that proxy holds them
        // without a request.
        let proxy_url = "http://127.0.0.1:9/plugin-store-clients-share-proxy-cooldown";
        let token_env = "HOME_PLUGIN_NETWORK_SCOPE_TEST_TOKEN";
        let token = "plugin_store_clients_share_proxy_cooldown";
        // SAFETY: the variable is unique to this test.
        unsafe { std::env::set_var(token_env, token) };
        let resolved = vec![ResolvedAuthConfig {
            matches: "https://api.github.com/".into(),
            auth_type: "github-token".into(),
            token: Secret(token.as_bytes().to_vec()),
            ..Default::default()
        }];
        let expires = SystemTime::now() + Duration::from_secs(3600);
        let plugin = Plugin {
            repository: "https://github.com/test/home-network-scope".into(),
            ..Default::default()
        };
        let seed_doer = Arc::new(RetryAfterDoer);
        let mut seed = Client::new(seed_doer);
        seed.network_scope = proxy_url.into();
        seed.resolved_auth = resolved.clone();
        seed.resolved_auth_expires_at = Some(expires);
        assert!(matches!(
            seed.fetch_latest_release(&plugin).await,
            Err(StoreError::RateLimited(_))
        ));

        let document: Value = serde_yaml_ng::from_str(&format!(
            "requests:\n  proxy-url: \" {proxy_url} \"\nplugins:\n  store-auth:\n    - match: https://api.github.com/\n      type: github-token\n      token-env: {token_env}\n"
        ))
        .unwrap();
        let cfg = SyncConfig::from_document(&document);
        let proxy = Arc::new(MapDoer {
            status: Some(502),
            ..Default::default()
        });
        let http: Arc<dyn Doer> = proxy.clone();
        let mut configured = store_client(&cfg, &http);
        configured.auth = cfg.store_auth.clone();
        let mut resolved_client = store_client(&cfg, &http);
        resolved_client.resolved_auth = resolved;
        resolved_client.resolved_auth_expires_at = Some(expires);
        for client in [configured, resolved_client] {
            let result = client.fetch_latest_release(&plugin).await;
            assert!(matches!(result, Err(StoreError::RateLimited(_))), "{result:?}");
        }
        assert_eq!(proxy.calls.load(Ordering::SeqCst), 0);
    }

    /// GitHub's 429 with an hour's `Retry-After`.
    struct RetryAfterDoer;

    impl Doer for RetryAfterDoer {
        fn get(&self, _url: &str, _headers: &Headers) -> BoxFuture<'_, Result<DoerResponse, String>> {
            Box::pin(async {
                Ok(DoerResponse {
                    status: 429,
                    headers: Headers::from([("Retry-After".to_owned(), vec!["3600".to_owned()])]),
                    body: futures_util::stream::once(async { Ok(Bytes::from_static(b"limited")) }).boxed(),
                })
            })
        }
    }

    fn status_report(plugins: Vec<PluginInstallStatus>) -> SyncReport {
        SyncReport {
            task: TASK_SYNC.into(),
            status: STATUS_OK.into(),
            ok: true,
            plugins: NonNil(plugins),
            ..Default::default()
        }
    }

    fn sample(install_status: &str) -> PluginInstallStatus {
        PluginInstallStatus {
            id: "sample".into(),
            install_status: install_status.into(),
            ..Default::default()
        }
    }

    async fn report_and_capture(node_id: &str, report: SyncReport, fail: bool) -> (Result<(), String>, Vec<u8>) {
        let captured = Mutex::new(Vec::new());
        let result = report_plugin_status(
            |payload| {
                *captured.lock().unwrap() = payload;
                async move { if fail { Err("push failed".to_owned()) } else { Ok(()) } }
            },
            node_id,
            report,
        )
        .await;
        (result, captured.into_inner().unwrap())
    }

    #[tokio::test]
    async fn report_plugin_status_pushes_node_report() {
        let (result, payload) = report_and_capture(" node-1 ", status_report(vec![sample(INSTALLED)]), false).await;
        result.unwrap();
        let payload: SyncReport = gojson::from_slice(&payload).unwrap();
        assert_eq!(payload.node_id, "node-1");
        assert!(payload.ok);
        assert_eq!(payload.plugins.0.len(), 1);
        assert!(payload.updated_at.0.is_some());
    }

    #[tokio::test]
    async fn report_plugin_status_pushes_empty_report() {
        let (result, payload) = report_and_capture("node-1", status_report(Vec::new()), false).await;
        result.unwrap();
        // Go marshals an empty, non-nil slice as [].
        assert!(String::from_utf8_lossy(&payload).contains(r#""plugins":[]"#));
        let payload: SyncReport = gojson::from_slice(&payload).unwrap();
        assert_eq!(payload.node_id, "node-1");
        assert!(payload.plugins.0.is_empty());
    }

    #[tokio::test]
    async fn report_plugin_status_requires_node_id() {
        let report = SyncReport {
            plugins: NonNil(vec![sample(FAILED)]),
            ..Default::default()
        };
        let (result, payload) = report_and_capture(" ", report, false).await;
        assert!(result.unwrap_err().contains("node id"));
        assert!(payload.is_empty());
    }

    #[tokio::test]
    async fn report_plugin_status_propagates_push_error() {
        let report = SyncReport {
            plugins: NonNil(vec![sample(INSTALLED)]),
            ..Default::default()
        };
        let (result, _) = report_and_capture("node-1", report, true).await;
        assert_eq!(result.unwrap_err(), "push failed");
    }

    fn tempdir() -> PathBuf {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let dir = std::env::temp_dir().join(format!(
            "cpa-homeplugins-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::SeqCst)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }
}
