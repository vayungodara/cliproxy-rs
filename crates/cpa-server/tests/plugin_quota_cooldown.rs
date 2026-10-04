//! A plugin quota reset clears the credential's routing cooldowns and persists the
//! cleared state to the configured cooldown backend, as Go's `Manager.ResetQuota` does
//! through `persistCooldownStates` (sdk/cliproxy/auth/conductor_cooldown.go). Without
//! that save a restart would restore the cooldown from the backend.

#![cfg(unix)]

use std::net::SocketAddr;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use cpa_core::config::Config;
use cpa_exec::Executors;
use cpa_exec::claude::ClaudeExecutor;
use cpa_server::cooldown_store::{Backend, Quota, Record};
use cpa_server::management::{self, Management, Options};

/// Loads one cooldown for `claude.json` and keeps every save.
struct FakeBackend {
    saves: Mutex<Vec<Vec<Record>>>,
}

impl Backend for FakeBackend {
    fn load(&self) -> Result<Vec<Record>, String> {
        Ok(vec![Record {
            provider: "claude".into(),
            auth_id: "claude.json".into(),
            status: "error".into(),
            next_retry_after: Some(SystemTime::now() + Duration::from_secs(3600)),
            reason: "quota".into(),
            quota: Quota {
                exceeded: true,
                ..Default::default()
            },
            ..Default::default()
        }])
    }

    fn save(&self, records: Vec<Record>, _now: SystemTime) -> Result<(), String> {
        self.saves.lock().unwrap().push(records);
        Ok(())
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn plugin_quota_reset_persists_cleared_cooldowns_to_the_backend() {
    let tmp = Path::new(env!("CARGO_TARGET_TMPDIR"));
    let test = "plugin_quota_cooldown::plugin_quota_reset_persists_cleared_cooldowns_to_the_backend";
    let Some(built) = cpa_plugin::testing::built_plugins(tmp, test) else {
        return;
    };
    let work = cpa_plugin::testing::scratch(tmp, "plugin-quota-cooldown");
    let (plugins, records, auths) = (work.join("plugins"), work.join("records"), work.join("auths"));
    for dir in [&plugins, &records.join("respond").join("a"), &auths] {
        std::fs::create_dir_all(dir).unwrap();
    }
    std::fs::copy(built.join("recorder.so"), plugins.join("recorder-a.so")).unwrap();
    std::fs::write(
        records.join("respond/a/quota.reset.json"),
        r#"{"ok":true,"result":{"success":true}}"#,
    )
    .unwrap();
    // A fake credential; nothing is sent anywhere.
    std::fs::write(
        auths.join("claude.json"),
        r#"{"type":"claude","email":"a@example.invalid","access_token":"fake-access"}"#,
    )
    .unwrap();
    let hash = bcrypt::hash("fake-secret", 4).unwrap();
    let config_path = work.join("config.yaml");
    std::fs::write(
        &config_path,
        format!(
            "config-version: 8\nauth-dir: {auths}\nrouting:\n  cooldown:\n    save-cooldown-status: true\n\
             management:\n  secret-key: {hash}\nplugins:\n  enabled: true\n  dir: {plugins}\n  configs:\n    \
             recorder-a:\n      enabled: true\n      record: {records}\n      label: a\n      caps: quota_provider\n",
            auths = auths.display(),
            plugins = plugins.display(),
            records = records.display(),
        ),
    )
    .unwrap();
    let config = Config::load(&config_path).unwrap();
    let credentials = cpa_core::config::credentials::load(&config);
    let credential = credentials.iter().find(|c| c.id == "claude.json").unwrap().clone();
    let index = cpa_core::config::credentials::auth_index(&credential);
    let rt = Arc::new(cpa_server::testing::runtime(
        config,
        credentials,
        Executors {
            claude: ClaudeExecutor::new("http://127.0.0.1:1").unwrap(),
            codex: Default::default(),
            devices: Default::default(),
            openai: Default::default(),
            google: Default::default(),
        },
    ));
    let backend = Arc::new(FakeBackend {
        saves: Mutex::default(),
    });
    rt.set_cooldown_backend(backend.clone());
    assert!(
        !rt.store().cooldowns("claude.json").is_empty(),
        "the backend's cooldown was restored"
    );
    let saves_before = backend.saves.lock().unwrap().len();

    let state = Management::with_options(rt.clone(), config_path, Options::default());
    cpa_server::plugins::start(&rt).await;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let app = management::router(state).layer(axum::middleware::from_fn(
        |mut req: axum::extract::Request, next: axum::middleware::Next| async move {
            let peer: SocketAddr = "127.0.0.1:40000".parse().unwrap();
            req.extensions_mut().insert(axum::extract::ConnectInfo(peer));
            next.run(req).await
        },
    ));
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

    let res = wreq::Client::new()
        .post(format!("{base}/v0/management/plugins/recorder-a/quota/reset"))
        .header("Authorization", "Bearer fake-secret")
        .body(format!(r#"{{"auth_index":"{index}"}}"#))
        .send()
        .await
        .unwrap();
    let status = res.status().as_u16();
    let body = res.text().await.unwrap();
    assert_eq!(status, 200, "{body}");
    assert!(
        rt.store().cooldowns("claude.json").is_empty(),
        "the reset cleared the cooldown"
    );
    {
        let saves = backend.saves.lock().unwrap();
        assert!(saves.len() > saves_before, "the cleared state was saved to the backend");
        assert!(
            saves.last().unwrap().iter().all(|r| r.auth_id != "claude.json"),
            "the last save no longer holds the credential's cooldown"
        );
    }
    rt.plugins().shutdown_all(None).await;
    let _ = std::fs::remove_dir_all(&work);
}
