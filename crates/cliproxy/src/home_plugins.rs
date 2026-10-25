//! Home-managed plugins on a node (Go sdk/cliproxy/home_plugins.go and the plugin part
//! of cmd/server/main.go's Home bootstrap): each Home config's plugins are synced
//! before the config applies, Home's delete tasks run after it applied, and every
//! outcome is reported to Home's `plugin-status` list.

use std::fmt::Write as _;
use std::sync::{Arc, LazyLock, Mutex, PoisonError};
use std::time::Duration;

use cpa_exec::proxy::{GoClients, Proxy};
use cpa_home::Client;
use cpa_home::client::PluginTask;
use cpa_plugin::gojson;
use cpa_plugin::homeplugins::{self, PluginRuntime, SyncConfig, SyncReport};
use cpa_plugin::store::{self, Doer, PluginSyncRequest, PluginSyncResponse};
use tokio_util::sync::CancellationToken;

/// Go main's 30-second context around the bootstrap sync.
const BOOTSTRAP_TIMEOUT: Duration = Duration::from_secs(30);

/// The store's HTTP clients, one per proxy (Go builds an `http.Client` per sync).
static CLIENTS: LazyLock<GoClients> = LazyLock::new(|| GoClients::new(Default::default()));

fn doer(cfg: &SyncConfig) -> Arc<dyn Doer> {
    Arc::new(store::WreqDoer::new(CLIENTS.get(&Proxy::parse(&cfg.proxy_url))))
}

/// The plugin config Home sent: `cfg` as the node runs it, with the `plugins.store-auth`
/// that Home mode strips from it before applying (Go syncs with a copy that keeps it).
pub fn sync_config(raw: &[u8], cfg: &cpa_core::config::Config) -> SyncConfig {
    let mut out = SyncConfig::from_document(&cfg.document);
    // As Go's typed decode reads it: `!!binary` values decoded, merges expanded.
    let document = std::str::from_utf8(raw).ok().and_then(|text| {
        let mut value = cpa_core::config::config_value(text).ok()?;
        cpa_core::config::expand_merges(&mut value).ok()?;
        Some(value)
    });
    if let Some(document) = document {
        out.store_auth = cpa_plugin::config::store_auth(&document);
    }
    out
}

/// Go `homePluginSyncKey`: what decides whether a config needs a new plugin sync.
fn sync_key(cfg: &SyncConfig) -> String {
    let mut key = format!(
        "enabled={}\ndir={}\nauth-revision={}\n",
        cfg.enabled,
        cfg.dir.trim(),
        cfg.auth_revision
    );
    for (id, node) in &cfg.configs {
        let item = cpa_plugin::config::item_config(id, node, None);
        let _ = writeln!(
            key,
            "plugin={}\nenabled={}\npriority={}",
            id.trim(),
            item.enabled,
            item.priority
        );
        if !node.is_null() {
            key.push_str(&serde_yaml_ng::to_string(node).unwrap_or_default());
        }
        key.push('\n');
    }
    key
}

/// Go `GetPluginSync`'s decoding: the raw response is zeroed once decoded, since it
/// carries store credentials.
async fn fetch(client: &Client, request: &PluginSyncRequest) -> Result<PluginSyncResponse, cpa_home::Error> {
    let mut raw = client.get_plugin_sync(&gojson::to_vec(request)).await?;
    let response = gojson::from_slice::<PluginSyncResponse>(&raw)
        .map_err(|e| cpa_home::Error::Other(format!("decode plugin sync response: {e}")));
    raw.fill(0);
    let response = response?;
    response
        .validate(gojson::GoTime::now_utc().0.expect("now"))
        .map_err(cpa_home::Error::Other)?;
    Ok(response)
}

/// Go `syncHomePluginsWithClient` without the unchanged-key check: a sync report,
/// whether plugins were installed (so the host must be asked what loaded), and the
/// sync error.
async fn sync_plugins(
    cfg: &SyncConfig,
    client: &Client,
    runtime: Option<&Arc<dyn PluginRuntime>>,
) -> (SyncReport, bool, Option<String>) {
    let platform = homeplugins::current_platform();
    if !cfg.enabled {
        return (homeplugins::completed_sync_report(&platform, None), false, None);
    }
    let installed = match homeplugins::installed_versions(cfg) {
        Ok(installed) => installed,
        Err(e) => return (homeplugins::completed_sync_report(&platform, Some(&e)), false, Some(e)),
    };
    let request = PluginSyncRequest {
        schema_version: store::PLUGIN_SYNC_SCHEMA_VERSION,
        goos: platform.goos.clone(),
        goarch: platform.goarch.clone(),
        installed_versions: installed,
    };
    match fetch(client, &request).await {
        Ok(response) => {
            let (report, error) = homeplugins::sync_resolved_with_report(
                cfg,
                response.items,
                response.expires_at,
                &request.installed_versions,
                &doer(cfg),
                runtime,
            )
            .await;
            (report, true, error)
        }
        Err(cpa_home::Error::PluginSyncUnsupported(_)) => {
            let (report, error) = homeplugins::sync_with_report(cfg, &doer(cfg), runtime).await;
            (report, true, error)
        }
        Err(e) => {
            let e = e.to_string();
            (homeplugins::completed_sync_report(&platform, Some(&e)), false, Some(e))
        }
    }
}

/// Go `pushHomePluginStatusWithClient`.
async fn push_status(client: &Client, node_id: &str, report: SyncReport) -> Result<(), String> {
    homeplugins::report_plugin_status(
        |payload| async move { client.rpush_plugin_status(&payload).await.map_err(|e| e.to_string()) },
        node_id,
        report,
    )
    .await
    .map_err(|e| format!("push home plugin status: {e}"))
}

/// A sync report still to push (Go `homePluginStatusWork`).
struct StatusWork {
    report: SyncReport,
    needs_load_marking: bool,
}

/// A Home plugin task still to run or report (Go `homePluginTaskWork`).
struct TaskWork {
    cfg: SyncConfig,
    task: PluginTask,
    report: Option<SyncReport>,
}

/// What a staged Home config leaves to do once it applied (Go `homePluginFinalization`).
/// Retrying it resumes where the last attempt stopped: a delete runs once, and only its
/// report is pushed again.
#[derive(Default)]
pub struct Finalization {
    statuses: Vec<StatusWork>,
    next_status: usize,
    tasks: Vec<TaskWork>,
    next_task: usize,
    sync_key: Option<String>,
}

/// A node's Home plugin state across Home lifetimes.
pub struct HomePlugins {
    node_id: String,
    runtime: Option<Arc<dyn PluginRuntime>>,
    /// Go `homePluginSyncKey`: the config last synced and reported.
    synced: Mutex<Option<String>>,
}

impl HomePlugins {
    pub fn new(node_id: &str, host: Option<cpa_plugin::Host>) -> Self {
        Self {
            node_id: node_id.trim().to_owned(),
            runtime: host.map(|host| Arc::new(host) as Arc<dyn PluginRuntime>),
            synced: Mutex::new(None),
        }
    }

    /// Go `stageHomeOverlayWithClient`'s plugin part: syncs the config's plugins unless
    /// they are unchanged since the last reported sync, and stages Home's delete tasks.
    /// An error leaves the config unapplied.
    pub async fn stage(&self, cfg: &SyncConfig, client: &Client) -> Result<Finalization, String> {
        let mut work = Finalization::default();
        let key = sync_key(cfg);
        let unchanged = self.synced.lock().unwrap_or_else(PoisonError::into_inner).as_ref() == Some(&key);
        if !unchanged {
            let (report, did_sync, error) = sync_plugins(cfg, client, self.runtime.as_ref()).await;
            if let Some(e) = error {
                return Err(format!("sync home plugins: {e}"));
            }
            work.sync_key = Some(key);
            if !self.node_id.is_empty() {
                work.statuses.push(StatusWork {
                    report,
                    needs_load_marking: did_sync,
                });
            }
        }
        work.tasks = self
            .stage_tasks(cfg, client)
            .await
            .map_err(|e| format!("stage home plugin tasks: {e}"))?;
        Ok(work)
    }

    /// Go `stageHomePluginTasksWithClient`: Home's delete tasks, run at finalization.
    async fn stage_tasks(&self, cfg: &SyncConfig, client: &Client) -> Result<Vec<TaskWork>, cpa_home::Error> {
        Ok(client
            .get_plugin_tasks()
            .await?
            .into_iter()
            .filter(|task| task.operation.trim().eq_ignore_ascii_case("delete"))
            .map(|task| TaskWork {
                cfg: cfg.clone(),
                task,
                report: None,
            })
            .collect())
    }

    /// Go `finalizeHomePluginWork`: pushes each sync report (with the plugins that
    /// loaded), runs and reports each delete task, then records the config as synced.
    pub async fn finalize(
        &self,
        cancel: &CancellationToken,
        client: &Client,
        work: &mut Finalization,
    ) -> Result<(), String> {
        const CANCELED: &str = "context canceled";
        if cancel.is_cancelled() {
            return Err(CANCELED.into());
        }
        while let Some(status) = work.statuses.get_mut(work.next_status) {
            if status.needs_load_marking {
                if let Some(e) = homeplugins::mark_load_results(&mut status.report, self.runtime.as_deref()) {
                    tracing::warn!("failed to load home plugins: {e}");
                }
                status.needs_load_marking = false;
            }
            push_status(client, &self.node_id, status.report.clone()).await?;
            work.next_status += 1;
        }
        while let Some(task) = work.tasks.get_mut(work.next_task) {
            let report = match &task.report {
                Some(report) => report.clone(),
                None => {
                    let report = homeplugins::delete_with_report(
                        cancel,
                        Some(&task.cfg),
                        self.runtime.as_ref(),
                        task.task.id,
                        &task.task.plugin_id,
                    )
                    .await;
                    if !report.ok && !report.error.trim().is_empty() {
                        tracing::warn!(
                            "failed to process home plugin delete task {} for {}: {}",
                            task.task.id,
                            task.task.plugin_id,
                            report.error
                        );
                    }
                    task.report.insert(report).clone()
                }
            };
            push_status(client, &self.node_id, report).await?;
            work.next_task += 1;
        }
        if let Some(key) = work.sync_key.take() {
            if cancel.is_cancelled() {
                work.sync_key = Some(key);
                return Err(CANCELED.into());
            }
            *self.synced.lock().unwrap_or_else(PoisonError::into_inner) = Some(key);
        }
        Ok(())
    }
}

/// The bootstrap's sync report, pushed again once the plugin host applied the config
/// (Go main's `MarkLoadResults` after `ApplyConfig`).
pub struct Bootstrap {
    client: Client,
    node_id: String,
    report: SyncReport,
}

/// Go main's Home plugin bootstrap: syncs the plugins Home's config names and reports
/// the outcome. The error is the line Go logs before exiting; the client is closed
/// either way once nothing more is to be reported.
pub async fn bootstrap(
    client: Client,
    node_id: &str,
    cfg: &SyncConfig,
    host: &cpa_plugin::Host,
) -> Result<Bootstrap, String> {
    let runtime: Arc<dyn PluginRuntime> = Arc::new(host.clone());
    let (report, _, error) = tokio::time::timeout(BOOTSTRAP_TIMEOUT, sync_plugins(cfg, &client, Some(&runtime)))
        .await
        .unwrap_or_else(|_| {
            let e = "context deadline exceeded".to_owned();
            let report = homeplugins::completed_sync_report(&homeplugins::current_platform(), Some(&e));
            (report, false, Some(e))
        });
    if let Err(e) = push_status(&client, node_id, report.clone()).await {
        tracing::warn!("failed to report home plugin sync status: {e}");
    }
    match error {
        Some(e) => {
            client.close();
            Err(format!("failed to sync plugins from home: {e}"))
        }
        None => Ok(Bootstrap {
            client,
            node_id: node_id.to_owned(),
            report,
        }),
    }
}

impl Bootstrap {
    /// Reports which synced plugins the host loaded, then closes the bootstrap client.
    /// The error is the line Go logs before exiting.
    pub async fn finish(mut self, host: &cpa_plugin::Host) -> Result<(), String> {
        let loaded = homeplugins::mark_load_results(&mut self.report, Some(host as &dyn PluginRuntime));
        let pushed = push_status(&self.client, &self.node_id, self.report).await;
        self.client.close();
        if let Err(e) = pushed {
            tracing::warn!("failed to report home plugin load status: {e}");
        }
        match loaded {
            Some(e) => Err(format!("failed to load home plugins: {e}")),
            None => Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    //! Go sdk/cliproxy/home_plugins_test.go against a fake Home.

    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicUsize, Ordering};

    use cpa_core::gostr::GoStr;
    use cpa_home::fake::{self, FakeHome, Reply};

    use super::*;

    const EMPTY_PLAN: &str = r#"{"schema_version":1,"expires_at":"2999-01-01T00:00:00Z","items":[]}"#;

    /// A fake Home answering plugin sync with `sync`, plugin tasks with `tasks`, and
    /// failing the first `fail_statuses` status pushes.
    struct PluginHome {
        home: FakeHome,
        statuses: Arc<Mutex<Vec<String>>>,
    }

    impl PluginHome {
        async fn start(sync: &'static str, tasks: &'static str, fail_statuses: usize) -> Self {
            let statuses = Arc::new(Mutex::new(Vec::new()));
            let pushes = Arc::new(AtomicUsize::new(0));
            let home = FakeHome::start({
                let statuses = statuses.clone();
                move |args| match (args[0].go_lower().as_str(), args.get(1).map(String::as_str)) {
                    ("ping", _) => fake::raw("+PONG\r\n"),
                    ("get", Some("plugin-sync")) if sync.starts_with('-') => fake::raw(sync),
                    ("get", Some("plugin-sync")) => fake::bulk(sync),
                    ("get", Some("plugin-tasks")) => fake::bulk(tasks),
                    ("rpush", Some("plugin-status")) => {
                        if pushes.fetch_add(1, Ordering::SeqCst) < fail_statuses {
                            return fake::raw("-ERR blocked\r\n");
                        }
                        statuses.lock().unwrap().push(args[2].clone());
                        fake::raw(":1\r\n")
                    }
                    _ => fake::raw("-ERR unexpected\r\n"),
                }
            })
            .await;
            Self { home, statuses }
        }

        fn client(&self) -> Client {
            self.home.client()
        }

        /// The pushed reports, decoded.
        fn statuses(&self) -> Vec<SyncReport> {
            let statuses = self.statuses.lock().unwrap();
            statuses
                .iter()
                .map(|s| gojson::from_slice(s.as_bytes()).unwrap())
                .collect()
        }

        fn pushes(&self) -> usize {
            self.home.count("rpush")
        }
    }

    fn sync_cfg(enabled: bool, dir: &Path, configs: &str) -> SyncConfig {
        let text = format!(
            "plugins:\n  enabled: {enabled}\n  dir: {}\n  configs:{configs}\n",
            dir.display()
        );
        SyncConfig::from_document(&serde_yaml_ng::from_str(&text).unwrap())
    }

    fn plugins() -> HomePlugins {
        HomePlugins::new("node-1", None)
    }

    fn synced(plugins: &HomePlugins) -> Option<String> {
        plugins.synced.lock().unwrap().clone()
    }

    fn tempdir() -> PathBuf {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let dir = std::env::temp_dir().join(format!(
            "cpa-home-plugins-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::SeqCst)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// `<dir>/<goos>/<goarch>/<name><extension>` for this platform.
    fn plugin_file(dir: &Path, name: &str) -> PathBuf {
        let platform = homeplugins::current_platform();
        let path = dir.join(&platform.goos).join(&platform.goarch).join(format!(
            "{name}{}",
            if platform.goos == "darwin" { ".dylib" } else { ".so" }
        ));
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "dummy").unwrap();
        path
    }

    #[tokio::test]
    async fn sync_home_plugins_skips_unchanged_signature() {
        let home = PluginHome::start(EMPTY_PLAN, "", 0).await;
        let (plugins, client, cfg) = (plugins(), home.client(), sync_cfg(true, &tempdir(), " {}"));
        let mut work = plugins.stage(&cfg, &client).await.unwrap();
        assert_eq!(work.statuses.len(), 1);
        assert!(work.statuses[0].needs_load_marking && work.statuses[0].report.ok);
        plugins
            .finalize(&CancellationToken::new(), &client, &mut work)
            .await
            .unwrap();
        assert_eq!(synced(&plugins), Some(sync_key(&cfg)));

        let work = plugins.stage(&cfg, &client).await.unwrap();
        assert!(work.statuses.is_empty() && work.sync_key.is_none());
        assert_eq!(home.home.count("get") - 2, 1, "an unchanged config fetched a new plan");
    }

    #[tokio::test]
    async fn sync_home_plugins_fetch_failure_returns_failure_report() {
        let home = PluginHome::start("-ERR plugin sync unavailable\r\n", "", 0).await;
        let cfg = sync_cfg(true, &tempdir(), " {}");
        let (report, did_sync, error) = sync_plugins(&cfg, &home.client(), None).await;
        let error = error.unwrap();
        assert!(error.contains("plugin sync unavailable"), "{error}");
        assert!(!did_sync);
        assert_eq!((report.schema_version, report.task.as_str()), (1, "plugin-sync"));
        assert!(!report.ok);
        assert_eq!(report.error, error);
        // A failed sync leaves the config unstaged and unmarked.
        assert!(plugins().stage(&cfg, &home.client()).await.is_err());
    }

    #[tokio::test]
    async fn sync_home_plugins_falls_back_for_unsupported_home_protocol() {
        let home = PluginHome::start("-ERR plugin_sync_unsupported\r\n", "", 0).await;
        let cfg = sync_cfg(true, &tempdir(), " {}");
        let (report, did_sync, error) = sync_plugins(&cfg, &home.client(), None).await;
        assert_eq!(error, None);
        assert!(did_sync);
        assert!(report.ok && report.task == "plugin-sync");
    }

    #[tokio::test]
    async fn sync_home_plugins_skips_fetch_when_plugins_disabled() {
        let home = PluginHome::start("-ERR fetch should not be called\r\n", "", 0).await;
        let plugins = plugins();
        let work = plugins
            .stage(&sync_cfg(false, &tempdir(), " {}"), &home.client())
            .await
            .unwrap();
        assert_eq!(home.home.count("get"), 1, "only the plugin tasks are read");
        assert_eq!(work.statuses.len(), 1);
        assert!(!work.statuses[0].needs_load_marking);
        assert!(work.statuses[0].report.ok && work.statuses[0].report.task == "plugin-sync");
        // The caller marks the config once it reported it.
        assert_eq!(synced(&plugins), None);
    }

    #[tokio::test]
    async fn sync_home_plugins_skips_disabled_report_when_unchanged() {
        let home = PluginHome::start("-ERR fetch should not be called\r\n", "", 0).await;
        let (plugins, client, cfg) = (plugins(), home.client(), sync_cfg(false, &tempdir(), " {}"));
        let mut work = plugins.stage(&cfg, &client).await.unwrap();
        plugins
            .finalize(&CancellationToken::new(), &client, &mut work)
            .await
            .unwrap();
        let work = plugins.stage(&cfg, &client).await.unwrap();
        assert!(work.statuses.is_empty());
        assert_eq!(home.statuses().len(), 1);
    }

    #[tokio::test]
    async fn finalize_home_plugin_work_retries_failed_status_without_marking_synced() {
        let home = PluginHome::start(EMPTY_PLAN, "", 1).await;
        let (plugins, client, cfg) = (plugins(), home.client(), sync_cfg(false, &tempdir(), " {}"));
        let mut work = plugins.stage(&cfg, &client).await.unwrap();
        let cancel = CancellationToken::new();
        assert!(plugins.finalize(&cancel, &client, &mut work).await.is_err());
        assert_eq!((synced(&plugins), work.next_status), (None, 0));
        plugins.finalize(&cancel, &client, &mut work).await.unwrap();
        assert_eq!((synced(&plugins), work.next_status), (Some(sync_key(&cfg)), 1));
        plugins.finalize(&cancel, &client, &mut work).await.unwrap();
        assert_eq!(home.pushes(), 2);
    }

    #[tokio::test]
    async fn stage_home_plugin_tasks_defers_delete_until_finalization() {
        let dir = tempdir();
        let path = plugin_file(&dir, "plugin-a");
        let home = PluginHome::start(
            EMPTY_PLAN,
            r#"[{"id":7,"operation":"delete","plugin_id":"plugin-a"},{"id":8,"operation":"install","plugin_id":"plugin-a"}]"#,
            0,
        )
        .await;
        let (plugins, client) = (plugins(), home.client());
        let mut work = plugins.stage(&sync_cfg(false, &dir, " {}"), &client).await.unwrap();
        assert_eq!(work.tasks.len(), 1);
        assert_eq!(work.tasks[0].task.id, 7);
        assert!(path.exists(), "staging deleted the plugin");
        plugins
            .finalize(&CancellationToken::new(), &client, &mut work)
            .await
            .unwrap();
        assert!(!path.exists());
        let statuses = home.statuses();
        let delete = statuses.last().unwrap();
        assert_eq!(
            (delete.task.as_str(), delete.task_id, delete.node_id.as_str()),
            ("plugin-delete", 7, "node-1")
        );
        assert_eq!(delete.plugins.0[0].install_status, "deleted");
    }

    #[tokio::test]
    async fn finalize_home_plugin_task_status_retry_does_not_repeat_delete() {
        let dir = tempdir();
        let path = plugin_file(&dir, "plugin-b");
        let home = PluginHome::start(EMPTY_PLAN, "", 1).await;
        let (plugins, client) = (plugins(), home.client());
        let mut work = Finalization {
            tasks: vec![TaskWork {
                cfg: sync_cfg(false, &dir, " {}"),
                task: PluginTask {
                    id: 8,
                    operation: "delete".into(),
                    plugin_id: "plugin-b".into(),
                    ..Default::default()
                },
                report: None,
            }],
            ..Default::default()
        };
        let cancel = CancellationToken::new();
        assert!(plugins.finalize(&cancel, &client, &mut work).await.is_err());
        assert!(!path.exists());
        assert!(work.next_task == 0 && work.tasks[0].report.is_some());
        plugins.finalize(&cancel, &client, &mut work).await.unwrap();
        assert_eq!(work.next_task, 1);
        assert_eq!(home.pushes(), 2);
        // A repeated delete would have found nothing and reported "missing".
        assert_eq!(home.statuses()[0].plugins.0[0].install_status, "deleted");
    }

    #[test]
    fn home_plugin_sync_key_includes_credential_revision() {
        let dir = tempdir();
        let mut cfg = sync_cfg(true, &dir, " {}");
        let first = sync_key(&cfg);
        cfg.auth_revision = 2;
        assert_ne!(first, sync_key(&cfg));
    }

    #[test]
    fn force_home_runtime_config_clears_store_auth() {
        let raw = b"plugins:\n  store-auth:\n    - match: https://downloads.example/\n      type: bearer\n      token-env: PLUGIN_TOKEN\n";
        let cfg = crate::home::overlay(raw, None).unwrap();
        assert!(cpa_plugin::config::store_auth(&cfg.document).is_empty());
        // The plugin sync still sees it.
        assert_eq!(sync_config(raw, &cfg).store_auth.len(), 1);
    }

    /// Go decodes the rules into typed fields, so a `!!binary` env name is its decoded
    /// text, through an alias too, and merge keys expand.
    #[test]
    fn store_auth_rules_read_binary_values_decoded() {
        let raw = b"plugins:\n  store-auth:\n    - match: &m !!binary aHR0cHM6Ly9kb3dubG9hZHMuZXhhbXBsZS8=\n      type: bearer\n      token-env: !!binary UExVR0lOX1RPS0VO\n    - match: *m\n      type: basic\n      username-env: USER_ENV\n      password-env: PASS_ENV\n    - <<: [{match: https://a.example/, type: bearer, token-env: FIRST}, {token-env: SECOND}]\n      match: https://b.example/\n";
        let cfg = crate::home::overlay(raw, None).unwrap();
        let rules = sync_config(raw, &cfg).store_auth;
        let got: Vec<_> = rules
            .iter()
            .map(|r| (r.matches.as_str(), r.token_env.as_str(), r.username_env.as_str()))
            .collect();
        assert_eq!(
            got,
            [
                ("https://downloads.example/", "PLUGIN_TOKEN", ""),
                ("https://downloads.example/", "", "USER_ENV"),
                // yaml.v3 merges: the explicit key wins, then the earlier mapping.
                ("https://b.example/", "FIRST", "")
            ]
        );
    }

    #[tokio::test]
    async fn stage_home_overlay_installed_plugin_at_runtime_issue6225() {
        let dir = tempdir();
        let platform = homeplugins::current_platform();
        let extension = if platform.goos == "darwin" { ".dylib" } else { ".so" };
        std::fs::write(dir.join(format!("myplugin-v1.0.0{extension}")), "dummy").unwrap();
        let home = PluginHome::start(EMPTY_PLAN, "", 0).await;
        let plugins = HomePlugins::new("node-1", Some(cpa_plugin::Host::new()));
        let client = home.client();
        let cfg = sync_cfg(true, &dir, "\n    myplugin:\n      enabled: true");
        let mut work = plugins.stage(&cfg, &client).await.unwrap();
        assert_eq!(work.statuses.len(), 1);
        plugins
            .finalize(&CancellationToken::new(), &client, &mut work)
            .await
            .unwrap();
        assert_eq!(home.pushes(), 1);
        assert!(!work.statuses[0].needs_load_marking);
        let report = &work.statuses[0].report;
        assert_eq!(report.plugins.0[0].id, "myplugin");
        assert_eq!(report.plugins.0[0].load_status, "failed");
    }

    #[tokio::test]
    async fn start_home_subscriber_does_not_pre_mark_plugin_sync() {
        let home = PluginHome::start("-ERR plugin sync unavailable\r\n", "", 0).await;
        let plugins = plugins();
        assert_eq!(synced(&plugins), None);
        let _ = plugins.stage(&sync_cfg(true, &tempdir(), " {}"), &home.client()).await;
        assert_eq!(synced(&plugins), None);
    }

    /// Hangs every status push.
    async fn hanging_home() -> FakeHome {
        FakeHome::start(|args| match args[0].go_lower().as_str() {
            "ping" => fake::raw("+PONG\r\n"),
            "rpush" => Reply::Hang,
            _ => fake::raw("$-1\r\n"),
        })
        .await
    }

    #[tokio::test]
    async fn shutdown_cancels_blocked_home_plugin_finalization() {
        let home = hanging_home().await;
        let (plugins, client) = (plugins(), home.client());
        let mut work = plugins
            .stage(&sync_cfg(false, &tempdir(), " {}"), &client)
            .await
            .unwrap();
        let cancel = CancellationToken::new();
        let blocked = plugins.finalize(&cancel, &client, &mut work);
        let finalized = tokio::time::timeout(Duration::from_millis(200), blocked).await;
        assert!(finalized.is_err(), "the push did not block");
        cancel.cancel();
        assert!(plugins.finalize(&cancel, &client, &mut work).await.is_err());
        assert_eq!(synced(&plugins), None);
    }
}
