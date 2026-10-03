//! The remote-store hooks (PGSTORE/OBJECTSTORE/GITSTORE): what the watcher and the
//! management API report to the store, checked with a recording persister. Expected
//! calls follow Go internal/watcher (persistConfigAsync after an accepted reload,
//! "Sync auth <name>" / "Remove auth <name>") and the management `deleteTokenRecord`.
//! Local server only; no provider endpoints.
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use cpa_core::config::Config;
use cpa_exec::Executors;
use cpa_exec::claude::ClaudeExecutor;
use cpa_server::management::{self, Management};
use cpa_server::persist::StorePersister;
use cpa_server::{Runtime, watching};
use futures_util::future::BoxFuture;

#[derive(Debug, Clone, PartialEq, Eq)]
enum Call {
    Config,
    Auth(String, Vec<PathBuf>),
    Delete(PathBuf),
}

struct Recorder {
    dir: PathBuf,
    calls: Mutex<Vec<Call>>,
    fail_delete: bool,
}

impl StorePersister for Recorder {
    fn persist_config(&self) -> BoxFuture<'_, anyhow::Result<()>> {
        self.calls.lock().unwrap().push(Call::Config);
        Box::pin(async { Ok(()) })
    }

    fn persist_auth_files(&self, message: String, paths: Vec<PathBuf>) -> BoxFuture<'_, anyhow::Result<()>> {
        self.calls.lock().unwrap().push(Call::Auth(message, paths));
        Box::pin(async { Ok(()) })
    }

    fn delete_auth(&self, path: PathBuf) -> anyhow::Result<()> {
        self.calls.lock().unwrap().push(Call::Delete(path));
        if self.fail_delete {
            anyhow::bail!("postgres store: delete auth record: connection refused")
        }
        Ok(())
    }

    fn auth_dir(&self) -> PathBuf {
        self.dir.clone()
    }
}

struct Fixture {
    dir: PathBuf,
    mirror: PathBuf,
    rt: Arc<Runtime>,
    state: Arc<Management>,
    store: Arc<Recorder>,
}

const HASH_SECRET: &str = "fake-management-only";

fn config_text(dir: &Path, retry: u32) -> String {
    let hash = bcrypt::hash(HASH_SECRET, 4).unwrap();
    // `auth-dir` names a directory the store must override.
    format!(
        "host: 127.0.0.1\nport: 0\nremote-management:\n  secret-key: '{hash}'\nauth-dir: {}\nrequest-retry: {retry}\n",
        dir.join("ignored-auth").display()
    )
}

fn fixture(name: &str, fail_delete: bool) -> Fixture {
    let dir = std::env::temp_dir().join(format!("store-hooks-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let mirror = dir.join("pgstore/auths");
    std::fs::create_dir_all(&mirror).unwrap();
    std::fs::create_dir_all(dir.join("ignored-auth")).unwrap();
    std::fs::write(dir.join("ignored-auth/other.json"), r#"{"type":"claude"}"#).unwrap();
    let path = dir.join("config.yaml");
    std::fs::write(&path, config_text(&dir, 3)).unwrap();
    let mut cfg = Config::load(&path).unwrap();
    // What main does in store mode before the runtime exists.
    cfg.auth_dir = mirror.clone();
    let rt = Arc::new(cpa_server::testing::runtime(
        cfg,
        vec![],
        Executors {
            claude: ClaudeExecutor::new("http://127.0.0.1:1").unwrap(),
            codex: Default::default(),
            devices: Default::default(),
            openai: Default::default(),
            google: Default::default(),
        },
    ));
    let store = Arc::new(Recorder {
        dir: mirror.clone(),
        calls: Mutex::default(),
        fail_delete,
    });
    let options = management::Options {
        store: Some(store.clone()),
        ..Default::default()
    };
    let state = Management::with_options(rt.clone(), path, options);
    Fixture {
        dir,
        mirror,
        rt,
        state,
        store,
    }
}

impl Fixture {
    fn calls(&self) -> Vec<Call> {
        self.store.calls.lock().unwrap().clone()
    }

    async fn wait_for(&self, what: &str, done: impl Fn(&[Call]) -> bool) {
        let waited = tokio::time::timeout(Duration::from_secs(5), async {
            while !done(&self.calls()) {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await;
        assert!(waited.is_ok(), "{what}: calls so far {:?}", self.calls());
    }

    /// Long enough for the watcher to settle and reload (50ms polls, 150ms debounce).
    async fn settle(&self) {
        tokio::time::sleep(Duration::from_millis(700)).await;
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn watcher_reports_changes_to_the_store_and_keeps_its_auth_dir() {
    let f = fixture("watch", false);
    std::fs::write(f.mirror.join("seed.json"), r#"{"type":"claude","access_token":"fake"}"#).unwrap();
    let watcher = watching::start(&f.state);
    // The first snapshot is the store's own mirror: nothing to push.
    tokio::time::timeout(Duration::from_secs(5), async {
        while f.rt.store().snapshot().len() != 1 {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("seed credential published");
    f.settle().await;
    assert_eq!(f.calls(), vec![]);
    assert_eq!(f.rt.config().auth_dir, f.mirror, "auth-dir stays the store mirror");

    let a = f.mirror.join("a.json");
    std::fs::write(&a, r#"{"type":"codex","access_token":"fake"}"#).unwrap();
    f.wait_for("sync a.json", |c| {
        c.contains(&Call::Auth("Sync auth a.json".into(), vec![a.clone()]))
    })
    .await;

    // A config the reload accepts is pushed; auth-dir in it is still overridden.
    std::fs::write(f.dir.join("config.yaml"), config_text(&f.dir, 5)).unwrap();
    f.wait_for("config", |c| c.contains(&Call::Config)).await;
    assert_eq!(f.rt.config().routing.retry.request_retry, 5);
    assert_eq!(f.rt.config().auth_dir, f.mirror);
    assert_eq!(f.rt.store().snapshot().len(), 2, "ignored-auth/other.json never loads");

    // A rejected config is not pushed; files that are not JSON objects are skipped.
    std::fs::write(f.dir.join("config.yaml"), "port: [unclosed\n").unwrap();
    std::fs::write(f.mirror.join("junk.json"), "not json").unwrap();
    f.settle().await;
    let configs = f.calls().iter().filter(|c| **c == Call::Config).count();
    assert_eq!(configs, 1, "{:?}", f.calls());
    assert!(
        !f.calls()
            .iter()
            .any(|c| matches!(c, Call::Auth(m, _) if m.contains("junk")))
    );

    std::fs::remove_file(&a).unwrap();
    f.wait_for("remove a.json", |c| {
        c.contains(&Call::Auth("Remove auth a.json".into(), vec![a.clone()]))
    })
    .await;
    watcher.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn management_deletes_reach_the_store_and_its_errors_reach_the_client() {
    for fail in [false, true] {
        let f = fixture(if fail { "delete-fail" } else { "delete" }, fail);
        for name in ["a.json", "b.json", "c.json"] {
            std::fs::write(f.mirror.join(name), r#"{"type":"claude","access_token":"fake"}"#).unwrap();
        }
        watching::reload(&f.state).unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let app = management::router(f.state.clone());
        let server = tokio::spawn(async move {
            axum::serve(
                listener,
                app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
            )
            .await
            .unwrap()
        });
        let client = wreq::Client::new();
        let delete = |query: &str| {
            client
                .delete(format!("{base}/v0/management/auth-files?{query}"))
                .bearer_auth(HASH_SECRET)
                .send()
        };
        let response = delete("name=a.json").await.unwrap();
        let status = response.status().as_u16();
        let body: serde_json::Value = response.json().await.unwrap();
        // Go removes the file first, then the store record; a store failure is a 500
        // carrying the store's error.
        assert!(!f.mirror.join("a.json").exists());
        assert_eq!(f.calls(), vec![Call::Delete(f.mirror.join("a.json"))]);
        if fail {
            assert_eq!(status, 500, "{body}");
            assert_eq!(body["error"], "postgres store: delete auth record: connection refused");
        } else {
            assert_eq!((status, body["status"].as_str()), (200, Some("ok")), "{body}");
        }
        let response = delete("all=true").await.unwrap();
        let status = response.status().as_u16();
        let body: serde_json::Value = response.json().await.unwrap();
        if fail {
            // Go stops at the first store failure.
            assert_eq!(status, 500, "{body}");
            assert_eq!(f.calls().len(), 2);
        } else {
            assert_eq!((status, body["deleted"].as_u64()), (200, Some(2)), "{body}");
            let mut deleted: Vec<Call> = f.calls()[1..].to_vec();
            deleted.sort_by_key(|c| format!("{c:?}"));
            assert_eq!(
                deleted,
                vec![
                    Call::Delete(f.mirror.join("b.json")),
                    Call::Delete(f.mirror.join("c.json"))
                ]
            );
        }
        server.abort();
    }
}
