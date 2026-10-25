//! Model catalog sources at startup and on config reload (Go
//! sdk/cliproxy/service_catalogs_test.go TestServiceCatalogStartupAndConfigReload). Its
//! own test binary: the updaters publish into the process-wide Devin catalog.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use cpa_core::registry::devin;

fn runtime(auth: &Path, sources: [&Path; 3]) -> Arc<cpa_server::Runtime> {
    let executors = cpa_exec::Executors {
        claude: cpa_exec::claude::ClaudeExecutor::new("http://127.0.0.1:9").unwrap(),
        codex: Default::default(),
        devices: Default::default(),
        openai: Default::default(),
        google: Default::default(),
    };
    let rt = cpa_server::testing::runtime(config(auth, sources), Vec::new(), executors);
    Arc::new(rt)
}

fn config(auth: &Path, [catalog, codex, devin]: [&Path; 3]) -> cpa_core::config::Config {
    let yaml = format!(
        "auth-dir: '{}'\nmodels:\n  catalog: '{}'\n  codex-catalog: '{}'\n  devin-catalog: '{}'\n",
        auth.display(),
        catalog.display(),
        codex.display(),
        devin.display()
    );
    cpa_core::config::Config::parse(&yaml).unwrap()
}

async fn await_devin(want: &[u8]) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while devin::global().snapshot().0 != want {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the configured Devin catalog was not published"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// The tests share the process-wide Devin catalog.
static SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

#[tokio::test]
async fn service_catalog_startup_and_config_reload() {
    let _serial = SERIAL.lock().await;
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("..");
    let general = root.join("cpa-core/src/registry/models.json");
    let codex = root.join("cpa-common/src/codex_client_models.json");
    let dir = std::env::temp_dir().join(format!("catalog-sources-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    let write = |name: &str, data: &[u8]| {
        let path = dir.join(name);
        std::fs::write(&path, data).unwrap();
        path
    };
    let first = br#"{"devin":[{"id":"devin/sdk-startup-regression","display_name":"SDK startup"}]}"#;
    let second = br#"{"devin":[{"id":"devin/sdk-reload-regression","display_name":"SDK reload"}]}"#;
    let first_path = write("first.json", first);
    let auth = dir.join("auth");

    // Startup reads the configured source (a test runtime still reads explicit ones).
    let rt = runtime(&auth, [&general, &codex, &first_path]);
    cpa_server::model_updater::start(&rt, false);
    await_devin(first).await;

    // A published config with another source switches to it.
    let second_path = write("second.json", second);
    rt.publish_config(config(&auth, [&general, &codex, &second_path]));
    await_devin(second).await;

    // The same source restarts under a new runtime once the old one is gone.
    drop(rt);
    std::fs::write(&second_path, first).unwrap();
    let rt = runtime(&auth, [&general, &codex, &second_path]);
    cpa_server::model_updater::start(&rt, false);
    await_devin(first).await;

    // Restore the embedded catalog for anything else in this process.
    rt.publish_config(config(
        &auth,
        [&general, &codex, &write("embedded.json", devin::EMBEDDED)],
    ));
    await_devin(devin::EMBEDDED).await;
    let _ = std::fs::remove_dir_all(dir);
}

/// Each config commit reconfigures in commit order (Go `commitConfigUpdate`): switching
/// to B and back to A before anything else runs still reads A again, and a commit with
/// an invalid source is rejected whole.
#[tokio::test]
async fn commits_reconfigure_in_order_and_invalid_sources_are_rejected() {
    let _serial = SERIAL.lock().await;
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("..");
    let general = root.join("cpa-core/src/registry/models.json");
    let codex = root.join("cpa-common/src/codex_client_models.json");
    let dir = std::env::temp_dir().join(format!("catalog-commits-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    let catalog = |id: &str| format!(r#"{{"devin":[{{"id":"devin/{id}","display_name":"{id}"}}]}}"#).into_bytes();
    let (d0, d1, d2) = (catalog("commit-d0"), catalog("commit-d1"), catalog("commit-d2"));
    let (a, b) = (dir.join("a.json"), dir.join("b.json"));
    std::fs::write(&a, &d0).unwrap();
    std::fs::write(&b, &d2).unwrap();
    let auth = dir.join("auth");
    let rt = runtime(&auth, [&general, &codex, &a]);
    cpa_server::model_updater::start(&rt, false);
    await_devin(&d0).await;

    std::fs::write(&a, &d1).unwrap();
    rt.publish_config(config(&auth, [&general, &codex, &b]));
    rt.publish_config(config(&auth, [&general, &codex, &a]));
    await_devin(&d1).await;

    // A relative source fails Go's validation: the whole commit is rejected. Loaded
    // configs are validated already; this is a caller editing the document.
    let mut invalid = config(&auth, [&general, &codex, &b]);
    let models = invalid.document.get_mut("models").unwrap().as_mapping_mut().unwrap();
    models.insert("devin-catalog".into(), "relative.json".into());
    assert!(!rt.publish_config(invalid));
    assert_eq!(rt.config().catalog_sources()[2], a.display().to_string());

    // Restore the embedded catalog for the other test.
    let embedded = dir.join("embedded.json");
    std::fs::write(&embedded, devin::EMBEDDED).unwrap();
    rt.publish_config(config(&auth, [&general, &codex, &embedded]));
    await_devin(devin::EMBEDDED).await;
    let _ = std::fs::remove_dir_all(dir);
}
