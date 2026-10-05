//! Disposable local server only: no provider endpoints or credential exchanges.
use cpa_core::config::Config;
use cpa_exec::Executors;
use cpa_exec::claude::ClaudeExecutor;
use cpa_server::management::Management;
use cpa_server::{Runtime, management, watching};
use serde_json::{Value, json};
use std::path::PathBuf;
use std::sync::Arc;

struct Fixture {
    dir: PathBuf,
    state: Arc<Management>,
    rt: Arc<Runtime>,
}
#[tokio::test]
async fn codex_passive_quota_snapshots_appear_in_credential_entries() {
    let f = Fixture::from_yaml("codex-quota", |auth, hash| {
        std::fs::write(
            auth.join("codex-a.json"),
            r#"{"type":"codex","email":"a@example.invalid"}"#,
        )
        .unwrap();
        std::fs::write(
            auth.join("claude-b.json"),
            r#"{"type":"claude","email":"b@example.invalid"}"#,
        )
        .unwrap();
        format!(
            "config-version: 8\nmanagement:\n  secret-key: '{hash}'\noauth:\n  auth-dir: {}\n",
            auth.display()
        )
    });
    let snapshot = f.rt.store().snapshot();
    let id = |provider: &str| snapshot.iter().find(|c| c.provider == provider).unwrap().id.clone();
    let (codex, claude) = (id("codex"), id("claude"));
    let mut headers = axum::http::HeaderMap::new();
    headers.insert("x-codex-plan-type", "pro".parse().unwrap());
    headers.insert("x-codex-primary-used-percent", "12".parse().unwrap());
    headers.insert("x-unrelated", "1".parse().unwrap());
    // Signals recorded for another provider's credential never surface (Go keys
    // observation by provider).
    f.rt.executors.codex.quota().observe(&codex, "", &headers);
    f.rt.executors.codex.quota().observe(&claude, "", &headers);
    let (base, server) = f.server().await;
    let list = || async {
        wreq::Client::new()
            .get(format!("{base}/v8/management/credentials"))
            .bearer_auth("fake-management-only")
            .send()
            .await
            .unwrap()
            .json::<Value>()
            .await
            .unwrap()
    };
    let listed = list().await;
    let entry_in = |listed: &Value, name: &str| {
        listed["files"]
            .as_array()
            .unwrap()
            .iter()
            .find(|e| e["name"] == name)
            .unwrap()
            .clone()
    };
    let entry = |name: &str| entry_in(&listed, name);
    let quota = entry("codex-a.json")["quota"].clone();
    assert_eq!(
        quota["signals"],
        json!({"X-Codex-Plan-Type": "pro", "X-Codex-Primary-Used-Percent": "12"})
    );
    // Go time.Time JSON (RFC 3339 with an offset), close to now.
    let observed = chrono::DateTime::parse_from_rfc3339(quota["observed_at"].as_str().unwrap()).unwrap();
    assert!((chrono::Utc::now() - observed.to_utc()).num_seconds().abs() < 60);
    assert_eq!(entry("claude-b.json")["quota"], json!({"signals": {}}));
    assert!(entry("codex-a.json").get("model_quotas").is_none());

    // Per-model observations (Go model_quotas): keyed by the model without its
    // thinking suffix; the credential quota is the latest snapshot.
    let mut mini = axum::http::HeaderMap::new();
    mini.insert("x-codex-primary-used-percent", "70".parse().unwrap());
    f.rt.executors.codex.quota().observe(&codex, "gpt-5.4(high)", &headers);
    f.rt.executors.codex.quota().observe(&codex, "gpt-5.4-mini", &mini);
    f.rt.executors
        .codex
        .quota()
        .observe(&claude, "claude-sonnet-4-6", &headers);
    let listed = list().await;
    server.abort();
    let codex_entry = entry_in(&listed, "codex-a.json");
    let models = codex_entry["model_quotas"].as_object().unwrap();
    assert_eq!(models.keys().collect::<Vec<_>>(), ["gpt-5.4", "gpt-5.4-mini"]);
    assert_eq!(
        models["gpt-5.4"]["signals"],
        json!({"X-Codex-Plan-Type": "pro", "X-Codex-Primary-Used-Percent": "12"})
    );
    assert_eq!(
        models["gpt-5.4-mini"]["signals"],
        json!({"X-Codex-Primary-Used-Percent": "70"})
    );
    assert!(models["gpt-5.4-mini"]["observed_at"].is_string());
    assert_eq!(
        codex_entry["quota"]["signals"],
        json!({"X-Codex-Primary-Used-Percent": "70"})
    );
    assert!(entry_in(&listed, "claude-b.json").get("model_quotas").is_none());
}

impl Fixture {
    fn new(name: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("manage-{name}-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("auth")).unwrap();
        let path = dir.join("config.yaml");
        let hash = bcrypt::hash("fake-management-only", 4).unwrap();
        std::fs::write(&path, format!("# operator note\nconfig-version: 8\nserver:\n  host: '127.0.0.1' # listener\n  port: 0\nmanagement:\n  secret-key: '{hash}'\noauth:\n  auth-dir: {}\nrouting:\n  retry:\n    request-retry: 3 # attempts\naccess:\n  api-keys: [fake-client]\n", dir.join("auth").display())).unwrap();
        let cfg = Config::load(&path).unwrap();
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
        let state = Management::new(rt.clone(), path);
        Self { dir, state, rt }
    }
    /// A fixture whose runtime holds the credentials `yaml` synthesizes.
    fn from_yaml(name: &str, yaml: impl Fn(&std::path::Path, &str) -> String) -> Self {
        let dir = std::env::temp_dir().join(format!("manage-{name}-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("auth")).unwrap();
        let path = dir.join("config.yaml");
        let hash = bcrypt::hash("fake-management-only", 4).unwrap();
        std::fs::write(&path, yaml(&dir.join("auth"), &hash)).unwrap();
        let cfg = Config::load(&path).unwrap();
        let rt = Arc::new(cpa_server::testing::runtime(
            cfg.clone(),
            cpa_core::config::credentials::load(&cfg),
            Executors {
                claude: ClaudeExecutor::new("http://127.0.0.1:1").unwrap(),
                codex: Default::default(),
                devices: Default::default(),
                openai: Default::default(),
                google: Default::default(),
            },
        ));
        let state = Management::new(rt.clone(), path);
        Self { dir, state, rt }
    }
    async fn server(&self) -> (String, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let app = management::router(self.state.clone());
        let handle = tokio::spawn(async move {
            axum::serve(
                listener,
                app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
            )
            .await
            .unwrap();
        });
        (base, handle)
    }
    fn file(&self) -> String {
        std::fs::read_to_string(self.dir.join("config.yaml")).unwrap()
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

#[tokio::test]
async fn real_http_config_rejections_comments_publication_and_panel_assets() {
    let f = Fixture::new("http");
    let (base, server) = f.server().await;
    let client = wreq::Client::new();
    let request = |method, path: &str| {
        client
            .request(method, format!("{base}/v8/management{path}"))
            .bearer_auth("fake-management-only")
    };
    let before = f.file();
    assert_eq!(
        client
            .get(format!("{base}/v8/management/config"))
            .send()
            .await
            .unwrap()
            .status(),
        401
    );
    let config: Value = request(wreq::Method::GET, "/config")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(config["routing"]["retry"]["request-retry"], 3);
    assert_eq!(f.file(), before, "GET must not migrate/persist");
    for (path, value, status) in [
        ("/config/server/port", json!("bad"), 422),
        ("/config/access", Value::Null, 422),
        ("/config/oauth/providers/codex/unknown", json!(true), 400),
        (
            "/config/credentials/concurrency/lifecycle-config-revision",
            json!(5),
            400,
        ),
        ("/config/access/api-keys/0", json!("invalid-index"), 400),
        ("/config/routing/retry/request-retry", json!({"value":2}), 422),
    ] {
        let response = request(wreq::Method::PUT, path).json(&value).send().await.unwrap();
        assert_eq!(response.status().as_u16(), status, "{path}");
        assert_eq!(f.file(), before, "rejected {path} changed disk");
        assert_eq!(f.rt.config().routing.retry.request_retry, 3);
    }
    let r = request(wreq::Method::PUT, "/config/routing/retry/request-retry")
        .json(&0)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    assert_eq!(f.rt.config().routing.retry.request_retry, 0);
    let text = f.file();
    assert!(
        text.contains("# operator note") && text.contains("# listener") && text.contains("# attempts"),
        "comments disappeared"
    );
    assert!(text.find("server:").unwrap() < text.find("management:").unwrap());
    let r = request(wreq::Method::PATCH, "/config")
        .json(&json!({"routing":{"cooldown":{"disable-cooling":false}}, "access":{"api-keys":[]}}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    assert!(f.rt.config().api_keys.is_empty());
    assert_eq!(
        request(wreq::Method::DELETE, "/config/routing/retry/request-retry")
            .send()
            .await
            .unwrap()
            .status(),
        200
    );
    assert_eq!(
        request(wreq::Method::GET, "/config/routing/retry/request-retry")
            .send()
            .await
            .unwrap()
            .status(),
        404
    );
    let html = client.get(format!("{base}/management.html")).send().await.unwrap();
    assert_eq!(html.status(), 200);
    let html = html.text().await.unwrap();
    for piece in html
        .split('"')
        .filter(|p| p.starts_with("./assets/") || p.starts_with("./fonts/"))
    {
        assert_eq!(
            client
                .get(format!("{base}/{}", piece.trim_start_matches("./")))
                .send()
                .await
                .unwrap()
                .status(),
            200,
            "asset {piece}"
        );
    }
    server.abort();
}

#[tokio::test]
async fn watcher_keeps_last_good_config_and_reconciles_disabled_deleted_and_self_writes() {
    let f = Fixture::new("reload");
    let auth = f.dir.join("auth/fake.json");
    std::fs::write(
        &auth,
        r#"{"type":"claude","access_token":"fake-token","email":"fake@example.invalid","unknown":{"keep":7}}"#,
    )
    .unwrap();
    watching::reload(&f.state).unwrap();
    let revision = f.rt.store().get("fake.json").unwrap().revision;
    watching::reload(&f.state).unwrap();
    assert_eq!(f.rt.store().get("fake.json").unwrap().revision, revision);
    std::fs::write(&auth, "{incomplete").unwrap();
    watching::reload(&f.state).unwrap();
    assert_eq!(f.rt.store().get("fake.json").unwrap().revision, revision);
    let (base, server) = f.server().await;
    let client = wreq::Client::new();
    let value: Value = client
        .patch(format!("{base}/v8/management/credentials/status"))
        .bearer_auth("fake-management-only")
        .json(&json!({"name":"fake.json","disabled":true}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(value["disabled"], true);
    let saved: Value = serde_json::from_slice(&std::fs::read(&auth).unwrap()).unwrap();
    assert_eq!(saved["unknown"]["keep"], 7);
    let disabled_revision = f.rt.store().get("fake.json").unwrap().revision;
    watching::reload(&f.state).unwrap();
    assert!(f.rt.store().get("fake.json").unwrap().disabled);
    assert_eq!(f.rt.store().get("fake.json").unwrap().revision, disabled_revision);
    std::fs::write(f.dir.join("config.yaml"), "").unwrap();
    std::fs::remove_file(&auth).unwrap();
    watching::reload(&f.state).unwrap();
    assert!(
        f.rt.store().snapshot().is_empty(),
        "empty config must not retain deleted auth"
    );
    assert_eq!(f.rt.config().api_keys, ["fake-client"]);
    std::fs::write(&auth, serde_json::to_vec(&saved).unwrap()).unwrap();
    watching::reload(&f.state).unwrap();
    assert!(f.rt.store().get("fake.json").unwrap().disabled);
    std::fs::write(f.dir.join("config.yaml"), "access: null\n").unwrap();
    assert!(watching::reload(&f.state).is_err());
    assert_eq!(f.rt.config().api_keys, ["fake-client"]);
    std::fs::remove_file(&auth).unwrap();
    assert!(watching::reload(&f.state).is_err());
    assert!(
        f.rt.store().snapshot().is_empty(),
        "bad config must not retain deleted auth"
    );
    std::fs::write(
        f.dir.join("config.yaml"),
        format!(
            "oauth: {{auth-dir: {}}}\nrequest-retry: 7\n",
            f.dir.join("auth").display()
        ),
    )
    .unwrap();
    watching::reload(&f.state).unwrap();
    assert_eq!(f.rt.config().routing.retry.request_retry, 7);
    assert!(f.rt.store().snapshot().is_empty());
    server.abort();
}

#[tokio::test]
async fn remote_policy_does_not_trust_forwarded_headers_and_key_hashing_preserves_source() {
    let f = Fixture::new("auth");
    let (base, server) = f.server().await;
    let client = wreq::Client::new();
    let r = client
        .get(format!("{base}/v8/management/config"))
        .header("X-Forwarded-For", "203.0.113.5")
        .header("X-Management-Key", "fake-management-only")
        .send()
        .await
        .unwrap();
    assert_eq!(
        r.status(),
        200,
        "untrusted forwarded header must not change local identity"
    );
    for _ in 0..5 {
        assert_eq!(
            client
                .get(format!("{base}/v8/management/config"))
                .bearer_auth("wrong")
                .send()
                .await
                .unwrap()
                .status(),
            401
        );
    }
    assert_eq!(
        client
            .get(format!("{base}/v8/management/config"))
            .bearer_auth("fake-management-only")
            .send()
            .await
            .unwrap()
            .status(),
        403
    );
    server.abort();
    let path = f.dir.join("plaintext.yaml");
    std::fs::write(
        &path,
        "# keep\nremote-management:\n  secret-key: fake-plaintext # key note\nport: 0\n",
    )
    .unwrap();
    let cfg = Config::load(&path).unwrap();
    assert!(bcrypt::verify("fake-plaintext", &cfg.management.secret_key).unwrap());
    let text = std::fs::read_to_string(&path).unwrap();
    assert!(text.contains("# keep") && text.contains("# key note"));
    assert!(!text.contains("fake-plaintext"));
    assert!(
        text.contains("remote-management:"),
        "startup must not migrate legacy source"
    );
}

/// Unix only: the read-only case needs Unix directory permissions.
#[cfg(unix)]
#[test]
fn plaintext_secret_loads_from_a_read_only_config_and_inherited_keys() {
    use std::os::unix::fs::PermissionsExt;
    let dir = std::env::temp_dir().join(format!("manage-readonly-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("config.yaml");
    let text = "port: 8317\nremote-management:\n  secret-key: fake-plain-secret\n";
    std::fs::write(&path, text).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o444)).unwrap();
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o555)).unwrap();
    let loaded = Config::load(&path);
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
    // Go: hashed in memory, persistence failure ignored.
    let cfg = loaded.expect("read-only config must load");
    assert!(bcrypt::verify("fake-plain-secret", &cfg.management.secret_key).unwrap());
    assert_eq!(std::fs::read_to_string(&path).unwrap(), text, "file untouched");
    // Keys inherited through a merge (legacy and v8 layout, at the root or inside the
    // parent mapping) are persisted hashed, with no plaintext left in the merge.
    for text in [
        "<<: {remote-management: {secret-key: fake-merged-secret}}\n",
        "<<: {management: {secret-key: fake-merged-secret}}\n",
        "management:\n  <<: {secret-key: fake-merged-secret}\n",
        "remote-management:\n  <<: {secret-key: fake-merged-secret}\n  allow-remote: false\n",
    ] {
        let layout = text;
        std::fs::write(&path, text).unwrap();
        let cfg = Config::load(&path).unwrap();
        assert!(bcrypt::verify("fake-merged-secret", &cfg.management.secret_key).unwrap());
        let file = std::fs::read_to_string(&path).unwrap();
        assert!(!file.contains("fake-merged-secret"), "{layout}: {file}");
        assert_eq!(
            Config::load(&path).unwrap().management.secret_key,
            cfg.management.secret_key
        );
    }
    // A writable plain layout is persisted as a hash, comments kept.
    std::fs::write(&path, "# keep\nmanagement:\n  secret-key: fake-v8-secret # note\n").unwrap();
    let cfg = Config::load(&path).unwrap();
    let file = std::fs::read_to_string(&path).unwrap();
    assert!(file.contains("# keep") && file.contains("# note") && !file.contains("fake-v8-secret"));
    assert!(file.contains(&cfg.management.secret_key));
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn management_bodies_are_unbounded_but_the_open_callback_is_not() {
    let f = Fixture::new("body-limit");
    let (base, server) = f.server().await;
    let client = wreq::Client::new();
    let note = "x".repeat(3 * 1024 * 1024);
    let r = client
        .post(format!("{base}/v8/management/credentials?name=large.json"))
        .bearer_auth("fake-management-only")
        .header("Content-Type", "application/json")
        .body(format!(r#"{{"type":"codex","note":"{note}"}}"#))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200, "{}", r.text().await.unwrap());
    assert!(f.dir.join("auth/large.json").metadata().unwrap().len() > 3 * 1024 * 1024);
    let r = client
        .post(format!("{base}/v8/management/oauth/callback"))
        .header("Content-Type", "application/json")
        .body(format!(r#"{{"state":"s","code":"{note}"}}"#))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 413);
    server.abort();
}

/// Waits up to four seconds for `done`.
async fn eventually(done: impl Fn() -> bool) {
    tokio::time::timeout(std::time::Duration::from_secs(4), async {
        while !done() {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("condition not reached");
}

#[tokio::test]
async fn watcher_reconciles_auth_while_the_config_file_is_missing() {
    let f = Fixture::new("missing-config");
    let watcher = watching::start(&f.state);
    let auth = |name: &str| f.dir.join("auth").join(name);
    std::fs::write(auth("a.json"), r#"{"type":"claude","access_token":"fake-a"}"#).unwrap();
    eventually(|| f.rt.store().get("a.json").is_some()).await;
    // Go keeps serving and still applies auth events after the config disappears.
    let config = f.file();
    std::fs::remove_file(f.dir.join("config.yaml")).unwrap();
    std::fs::write(auth("b.json"), r#"{"type":"claude","access_token":"fake-b"}"#).unwrap();
    std::fs::remove_file(auth("a.json")).unwrap();
    eventually(|| f.rt.store().get("b.json").is_some() && f.rt.store().get("a.json").is_none()).await;
    assert_eq!(f.rt.config().api_keys, ["fake-client"], "last good config kept");
    // An unchanged rewrite (same bytes) is not a change; an edit to the restored
    // config is applied.
    std::fs::write(
        f.dir.join("config.yaml"),
        config.replace("[fake-client]", "[fake-next]"),
    )
    .unwrap();
    eventually(|| f.rt.config().api_keys == ["fake-next"]).await;
    let revision = f.rt.store().get("b.json").unwrap().revision;
    std::fs::write(auth("b.json"), r#"{"type":"claude","access_token":"fake-b"}"#).unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(400)).await;
    assert_eq!(f.rt.store().get("b.json").unwrap().revision, revision);
    // A changed auth file of the same size is noticed (racy timestamps re-hash).
    std::fs::write(auth("b.json"), r#"{"type":"claude","access_token":"fake-c"}"#).unwrap();
    eventually(|| f.rt.store().get("b.json").unwrap().revision != revision).await;
    watcher.abort();
}

#[tokio::test]
async fn polling_publishes_owner_legacy_layout_and_ignores_non_auth_directories() {
    let f = Fixture::new("polling");
    std::fs::create_dir_all(f.dir.join("auth/static")).unwrap();
    std::fs::create_dir_all(f.dir.join("auth/logs")).unwrap();
    let watcher = watching::start(&f.state);
    std::fs::write(
        f.dir.join("config.yaml"),
        format!("host: 127.0.0.1\nport: 0\nremote-management: {{allow-remote: false}}\nauth-dir: {}\napi-keys: [owner-fake-client]\nrequest-retry: 5\n", f.dir.join("auth").display()),
    ).unwrap();
    std::fs::write(
        f.dir.join("auth/fake.json"),
        r#"{"type":"claude","access_token":"fake","disabled":true}"#,
    )
    .unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(4), async {
        loop {
            if f.rt.config().routing.retry.request_retry == 5 && f.rt.store().snapshot().len() == 1 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(f.rt.config().api_keys, ["owner-fake-client"]);
    assert!(!f.rt.config().management.allow_remote);
    assert!(f.rt.store().get("fake.json").unwrap().disabled);
    assert!(f.file().contains("remote-management:"), "load must not migrate layout");
    watcher.abort();
}

#[tokio::test]
async fn nonlocal_socket_requires_allow_remote_even_with_a_valid_key() {
    let f = Fixture::new("nonlocal");
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/v8/management/config", listener.local_addr().unwrap());
    // Inject a nonlocal socket identity, not a forwarded header, into real HTTP.
    let app = management::router(f.state.clone()).layer(axum::middleware::from_fn(
        |mut req: axum::extract::Request, next: axum::middleware::Next| async move {
            let peer = req
                .headers()
                .get("X-Test-Peer")
                .and_then(|v| v.to_str().ok())
                .unwrap_or("203.0.113.8:3456")
                .parse::<std::net::SocketAddr>()
                .unwrap();
            req.extensions_mut().insert(axum::extract::ConnectInfo(peer));
            next.run(req).await
        },
    ));
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let client = wreq::Client::new();
    assert_eq!(
        client
            .get(&url)
            .bearer_auth("fake-management-only")
            .send()
            .await
            .unwrap()
            .status(),
        403
    );
    assert_eq!(
        client
            .get(&url)
            .header("X-Test-Peer", "[::ffff:127.0.0.1]:3456")
            .bearer_auth("fake-management-only")
            .send()
            .await
            .unwrap()
            .status(),
        200
    );
    let mut config = (*f.rt.config()).clone();
    config.management.allow_remote = true;
    f.rt.publish_config(config);
    assert_eq!(
        client
            .get(&url)
            .bearer_auth("fake-management-only")
            .send()
            .await
            .unwrap()
            .status(),
        200
    );
    assert_eq!(client.get(&url).send().await.unwrap().status(), 401);
    server.abort();
}

/// The binary's composition (`cpa_server::app`): API routes first, management and its
/// NoRoute behind them. The API keeps gin's NoRoute rules (bare 404, no `Allow`, HEAD on
/// `/healthz` only); management keeps its own HEAD handling, so a HEAD under
/// `/v0/management` reaches the guard on the way to plugin routes.
#[tokio::test]
async fn served_app_keeps_api_and_management_no_route_rules() {
    let f = Fixture::new("served-app");
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let app = cpa_server::app(f.rt.clone(), management::router(f.state.clone()));
    let server = tokio::spawn(async move {
        axum::serve(
            listener,
            app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
        )
        .await
        .unwrap()
    });
    let client = wreq::Client::new();
    let send = |method: wreq::Method, path: &str, key: Option<&str>| {
        let mut req = client.request(method, format!("{base}{path}"));
        if let Some(key) = key {
            req = req.bearer_auth(key);
        }
        async move { req.send().await.unwrap() }
    };
    let get = wreq::Method::GET;
    let head = wreq::Method::HEAD;
    assert_eq!(
        send(get.clone(), "/v8/management/config", Some("fake-management-only"))
            .await
            .status(),
        200
    );
    assert_eq!(send(get.clone(), "/v1/models", None).await.status(), 401);
    assert_eq!(send(get.clone(), "/v1/models", Some("fake-client")).await.status(), 200);
    for (method, path) in [
        (get.clone(), "/v1/chat/completions"),
        (head.clone(), "/v1/models"),
        (get.clone(), "/v2/nothing"),
        (head.clone(), "/v2/nothing"),
    ] {
        let res = send(method.clone(), path, Some("fake-client")).await;
        assert_eq!(res.status(), 404, "{method} {path}");
        assert!(
            res.headers().get("allow").is_none(),
            "{method} {path}: {:?}",
            res.headers()
        );
    }
    assert_eq!(send(head.clone(), "/healthz", None).await.status(), 200);
    // Go's NoRoute guards /v0/management before looking for a plugin route.
    assert_eq!(send(head.clone(), "/v0/management/nothing", None).await.status(), 401);
    let res = send(head, "/v0/management/nothing", Some("fake-management-only")).await;
    assert_eq!(res.status(), 404);
    server.abort();
}

#[tokio::test]
async fn config_api_keys_become_scheduled_credentials_on_write_and_reload() {
    use cpa_server::runtime::Selection;
    let f = Fixture::new("synth");
    let (base, server) = f.server().await;
    let client = wreq::Client::new();
    let put = |path: &str, value: Value| {
        client
            .put(format!("{base}/v8/management{path}"))
            .bearer_auth("fake-management-only")
            .json(&value)
    };
    let r = put(
        "/config/api-keys/claude",
        json!([{"name": "team", "base-url": "https://claude.example.invalid", "prefix": "team", "priority": 4,
                "keys": [{"api-key": "fake-key-a"}, {"api-key": "fake-key-b", "priority": 9, "weight": 0,
                          "disable-cooling": false, "excluded-models": ["Opus-*"]}]}]),
    )
    .send()
    .await
    .unwrap();
    assert_eq!(r.status(), 200);
    // Published immediately by the write, without waiting for the watcher.
    let claude: Vec<_> =
        f.rt.store()
            .snapshot()
            .into_iter()
            .filter(|c| c.provider == "claude")
            .collect();
    assert_eq!(claude.len(), 2);
    let b = claude.iter().find(|c| c.attributes["api_key"] == "fake-key-b").unwrap();
    assert!(b.id.starts_with("claude:apikey:"));
    assert_eq!(b.attributes["priority"], "9");
    assert_eq!(b.attributes["weight"], "0");
    assert_eq!(b.attributes["prefix"], "team");
    assert_eq!(b.attributes["base_url"], "https://claude.example.invalid");
    assert_eq!(b.attributes["excluded_models"], "opus-*");
    assert_eq!(b.metadata["disable_cooling"], false);
    // The scheduler honours the synthesized priority and prefix.
    let sel = |model: &str| Selection {
        provider: "claude".into(),
        model: model.into(),
        ..Selection::default()
    };
    let lease = f.rt.store().select(sel("team/claude-sonnet-4-6")).unwrap();
    assert_eq!(lease.credential.attributes["api_key"], "fake-key-b", "priority 9 wins");
    assert_eq!(lease.execution_model, "claude-sonnet-4-6");
    drop(lease);
    let lease = f.rt.store().select(sel("team/opus-4")).unwrap();
    assert_eq!(
        lease.credential.attributes["api_key"], "fake-key-a",
        "b excludes opus-*"
    );
    drop(lease);
    // Credentials listing never shows config API keys (Go lists files only).
    let listed: Value = client
        .get(format!("{base}/v8/management/credentials"))
        .bearer_auth("fake-management-only")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(listed["files"], json!([]));

    // OAuth request-scoped rules reach the policy; invalid rules are dropped.
    let r = put(
        "/config/oauth/request-scoped-errors",
        json!({"Claude": [{"status": 400, "match": [" overloaded "], "action": " STOP "},
                          {"status": 0, "match": ["x"], "action": "stop"}]}),
    )
    .send()
    .await
    .unwrap();
    assert_eq!(r.status(), 200);
    let rules = &f.rt.policy().oauth_request_scoped_errors["claude"];
    assert_eq!(rules.len(), 1);
    assert_eq!(
        (rules[0].r#match[0].as_str(), rules[0].action.as_str()),
        ("overloaded", "stop")
    );

    // A hand edit picked up by the watcher replaces the set; removed keys disappear.
    let text = f.file().replace("fake-key-a", "fake-key-c");
    std::fs::write(f.dir.join("config.yaml"), text).unwrap();
    watching::reload(&f.state).unwrap();
    let keys: Vec<String> =
        f.rt.store()
            .snapshot()
            .iter()
            .filter_map(|c| c.attributes.get("api_key").cloned())
            .collect();
    assert!(keys.contains(&"fake-key-c".to_owned()) && !keys.contains(&"fake-key-a".to_owned()));
    server.abort();
}

#[tokio::test]
async fn unreadable_auth_dir_never_blocks_secret_rotation_or_removal() {
    let f = Fixture::new("authfile");
    // A regular file where the auth directory should be: ReadDir fails, like Go's
    // synthesizer this is an empty file set and the config still publishes.
    let blocker = f.dir.join("not-a-dir");
    std::fs::write(&blocker, "x").unwrap();
    let text = f.file().replace(
        &f.dir.join("auth").display().to_string(),
        &blocker.display().to_string(),
    );
    std::fs::write(
        f.dir.join("config.yaml"),
        text + "claude-api-key: [{api-key: fake-cfg-key}]\n",
    )
    .unwrap();
    watching::reload(&f.state).unwrap();
    assert_eq!(f.rt.config().auth_dir, blocker);
    assert!(f.rt.store().snapshot().iter().any(|c| c.provider == "claude"));
    let (base, server) = f.server().await;
    let client = wreq::Client::new();
    let get = |key: &str| {
        client
            .get(format!("{base}/v8/management/config/server/port"))
            .bearer_auth(key)
            .send()
    };
    let r = client
        .put(format!("{base}/v8/management/config/management/secret-key"))
        .bearer_auth("fake-management-only")
        .json(&json!("fake-rotated"))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    assert_eq!(
        get("fake-management-only").await.unwrap().status(),
        401,
        "old key revoked"
    );
    assert_eq!(get("fake-rotated").await.unwrap().status(), 200);
    let text = f.file();
    assert!(!text.contains("fake-rotated"), "secret stored hashed");
    // Removing the secret by hand disables management entirely on the next reload.
    let mut doc: serde_yaml_ng::Value = serde_yaml_ng::from_str(&text).unwrap();
    doc["management"].as_mapping_mut().unwrap().remove("secret-key");
    std::fs::write(f.dir.join("config.yaml"), serde_yaml_ng::to_string(&doc).unwrap()).unwrap();
    watching::reload(&f.state).unwrap();
    let r = get("fake-rotated").await.unwrap();
    assert_eq!((r.status().as_u16(), r.text().await.unwrap()), (404, String::new()));
    server.abort();
}

/// Deliberate difference from Go, whose saver re-normalizes every OAuth map and
/// rewrites every typed value on any write: only what the request wrote is
/// normalized, so untouched keys stay byte-stable.
#[tokio::test]
async fn writes_normalize_only_what_they_touch() {
    let f = Fixture::new("scoped");
    let path = f.dir.join("config.yaml");
    let dirty = "  excluded-models:\n    Claude: [' Opus-X ', opus-x] # dirty\n    gemini: [a]\n  model-alias:\n    claude:\n      - {name: a, alias: a} # self alias\n";
    let text = f.file().replacen("oauth:\n", &format!("oauth:\n{dirty}"), 1)
        + "observability:\n  pprof:\n    addr: null # keep\n";
    std::fs::write(&path, &text).unwrap();
    let (base, server) = f.server().await;
    let client = wreq::Client::new();
    let send = |method, at: &str, body: &str| {
        client
            .request(method, format!("{base}/v8/management{at}"))
            .bearer_auth("fake-management-only")
            .body(body.to_owned())
            .send()
    };
    let r = send(
        wreq::Method::PUT,
        "/config/oauth/excluded-models/gemini",
        r#"[" B ", "b", "c"]"#,
    )
    .await
    .unwrap();
    assert_eq!(r.status(), 200);
    let file = f.file();
    assert!(file.contains("    Claude: [' Opus-X ', opus-x] # dirty\n"), "{file}");
    assert!(file.contains("      - {name: a, alias: a} # self alias\n"), "{file}");
    assert!(file.contains("    addr: null # keep\n"), "{file}");
    let config: Value = send(wreq::Method::GET, "/config", "")
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(config["oauth"]["excluded-models"]["gemini"], json!(["b", "c"]));
    let r = send(
        wreq::Method::PATCH,
        "/config",
        r#"{"routing":{"retry":{"request-retry":1}}}"#,
    )
    .await
    .unwrap();
    assert_eq!(r.status(), 200);
    let after_patch = f.file();
    assert!(after_patch.contains("    addr: null # keep\n"), "{after_patch}");
    assert!(
        after_patch.contains("    Claude: [' Opus-X ', opus-x] # dirty\n"),
        "{after_patch}"
    );
    let r = send(wreq::Method::PATCH, "/config", "{}").await.unwrap();
    assert_eq!(r.status(), 200);
    assert_eq!(f.file(), after_patch, "an empty PATCH writes nothing");
    server.abort();
}

/// The status toggle edits only the targeted key, located in the file it edits
/// (here reordered externally and not yet published), and keeps sibling text.
#[tokio::test]
async fn config_key_toggle_edits_only_the_target_key_in_the_current_file() {
    let groups = |first: &str, second: &str| {
        format!(
            "api-keys:\n  claude:\n    - base-url: https://claude.example.invalid # group one\n      keys:\n        {first}\n        {second}\n    - keys:\n        - api-key: fake-c # third\n"
        )
    };
    let (a, b) = ("- api-key: fake-a # first key", "- api-key: fake-b # second key");
    let f = Fixture::from_yaml("toggle", |auth, hash| {
        format!(
            "config-version: 8\nmanagement:\n  secret-key: '{hash}'\noauth:\n  auth-dir: {}\n{}",
            auth.display(),
            groups(a, b)
        )
    });
    let cfg = f.rt.config();
    let ids = cpa_core::config::credentials::from_config(&cfg);
    let id_b = ids
        .iter()
        .find(|c| c.attributes.get("api_key").map(String::as_str) == Some("fake-b"))
        .unwrap()
        .id
        .clone();
    // External edit: the two keys swap places; the watcher has not published it.
    let path = f.dir.join("config.yaml");
    let swapped = f.file().replace(&groups(a, b), &groups(b, a));
    assert_ne!(swapped, f.file());
    std::fs::write(&path, &swapped).unwrap();
    let (base, server) = f.server().await;
    let r = wreq::Client::new()
        .patch(format!("{base}/v8/management/credentials/status"))
        .bearer_auth("fake-management-only")
        .json(&json!({"name": id_b, "disabled": true}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let file = f.file();
    for kept in [
        "    - base-url: https://claude.example.invalid # group one\n",
        "        - api-key: fake-a # first key\n",
        "        - api-key: fake-c # third\n",
    ] {
        assert!(file.contains(kept), "lost {kept:?}:\n{file}");
    }
    let doc: serde_yaml_ng::Value = serde_yaml_ng::from_str(&file).unwrap();
    let keys = &doc["api-keys"]["claude"][0]["keys"];
    assert_eq!(keys[0]["api-key"], "fake-b");
    assert_eq!(keys[0]["excluded-models"], serde_yaml_ng::Value::from(vec!["*"]));
    assert!(
        keys[1].get("excluded-models").is_none(),
        "fake-a must stay enabled:\n{file}"
    );
    server.abort();
}

/// Go edits config API keys in memory only: attributes follow the patched metadata
/// and config.yaml is not written.
#[tokio::test]
async fn config_key_field_patch_updates_memory_only() {
    let f = Fixture::from_yaml("cfgfields", |auth, hash| {
        format!(
            "config-version: 8\nmanagement:\n  secret-key: '{hash}'\noauth:\n  auth-dir: {}\napi-keys:\n  claude:\n    - keys:\n        - api-key: fake-k\n",
            auth.display()
        )
    });
    let id = f.rt.store().snapshot()[0].id.clone();
    let before = f.file();
    let (base, server) = f.server().await;
    let r = wreq::Client::new()
        .patch(format!("{base}/v8/management/credentials/fields"))
        .bearer_auth("fake-management-only")
        .json(&json!({"name": id, "note": " updated ", "priority": "3", "headers": {"X-B": "2"}, "disabled": "true"}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let cred = f.rt.store().snapshot().into_iter().find(|c| c.id == id).unwrap();
    assert_eq!(cred.attributes.get("note").map(String::as_str), Some("updated"));
    assert_eq!(cred.attributes.get("priority").map(String::as_str), Some("3"));
    assert_eq!(cred.attributes.get("header:X-B").map(String::as_str), Some("2"));
    assert_eq!(cred.attributes.get("api_key").map(String::as_str), Some("fake-k"));
    assert!(cred.disabled);
    assert_eq!(f.file(), before, "config.yaml must not change");
    server.abort();
}

/// `/credentials/models` reports what the credential registers in the dynamic
/// registry (Go `GetModelsForClient`): config aliases here, nothing once disabled.
#[tokio::test]
async fn credential_models_come_from_registrations() {
    let f = Fixture::from_yaml("models", |auth, hash| {
        format!(
            "config-version: 8\nmanagement:\n  secret-key: '{hash}'\noauth:\n  auth-dir: {}\napi-keys:\n  claude:\n    - models: [{{name: claude-sonnet-4-6, alias: sonnet}}]\n      keys:\n        - api-key: fake-k\n",
            auth.display()
        )
    });
    let id = f.rt.store().snapshot()[0].id.clone();
    let (base, server) = f.server().await;
    let client = wreq::Client::new();
    let models = |client: wreq::Client, base: String, id: String| async move {
        let v: Value = client
            .get(format!("{base}/v8/management/credentials/models?name={id}"))
            .bearer_auth("fake-management-only")
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        v["models"]
            .as_array()
            .unwrap()
            .iter()
            .map(|m| m["id"].as_str().unwrap().to_owned())
            .collect::<Vec<_>>()
    };
    assert_eq!(models(client.clone(), base.clone(), id.clone()).await, ["sonnet"]);
    let r = client
        .patch(format!("{base}/v8/management/credentials/fields"))
        .bearer_auth("fake-management-only")
        .json(&json!({"name": id, "disabled": true}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    assert!(models(client, base, id).await.is_empty());
    server.abort();
}

/// A relative auth-dir: an unclaimed upload is one fallback credential with a
/// relative ID, and once a field patch makes it claimable it is exactly one
/// synthesized credential (no leftover absolute-path fallback).
#[tokio::test]
async fn fallback_with_relative_auth_dir_is_retired_once_synthesized() {
    let f = Fixture::from_yaml("relfallback", |auth, hash| {
        let cwd = std::env::current_dir().unwrap();
        let up = "../".repeat(cwd.components().count() - 1);
        let relative = std::path::Path::new(&up).join(auth.strip_prefix("/").unwrap());
        format!(
            "config-version: 8\nmanagement:\n  secret-key: '{hash}'\noauth:\n  auth-dir: {}\n",
            relative.display()
        )
    });
    assert!(f.rt.config().auth_dir.is_relative());
    let (base, server) = f.server().await;
    let client = wreq::Client::new();
    let call = |method: wreq::Method, path: &str, body: &str| {
        client
            .request(method, format!("{base}/v8/management{path}"))
            .bearer_auth("fake-management-only")
            .body(body.to_owned())
            .send()
    };
    let ids = |f: &Fixture| {
        f.rt.store()
            .snapshot()
            .iter()
            .map(|c| (c.id.clone(), c.provider.clone()))
            .collect::<Vec<_>>()
    };
    assert_eq!(
        call(wreq::Method::POST, "/credentials?name=a.json", "{}")
            .await
            .unwrap()
            .status(),
        200
    );
    assert_eq!(ids(&f), [("a.json".to_owned(), "unknown".to_owned())]);
    let r = call(
        wreq::Method::PATCH,
        "/credentials/fields",
        r#"{"name":"a.json","type":"claude"}"#,
    )
    .await
    .unwrap();
    assert_eq!(r.status(), 200);
    assert_eq!(ids(&f), [("a.json".to_owned(), "claude".to_owned())]);
    server.abort();
}

/// A config-backed field patch invalidates the cached registry: a new prefix is
/// routable at once.
#[tokio::test]
async fn config_key_prefix_patch_refreshes_the_registry() {
    let f = Fixture::from_yaml("cfgprefix", |auth, hash| {
        format!(
            "config-version: 8\nmanagement:\n  secret-key: '{hash}'\noauth:\n  auth-dir: {}\napi-keys:\n  claude:\n    - models: [{{name: claude-sonnet-4-6, alias: sonnet}}]\n      keys:\n        - api-key: fake-k\n",
            auth.display()
        )
    });
    let id = f.rt.store().snapshot()[0].id.clone();
    assert!(
        !f.rt.registry().ids().any(|m| m == "new/sonnet"),
        "warm cache without the prefix"
    );
    let (base, server) = f.server().await;
    let r = wreq::Client::new()
        .patch(format!("{base}/v8/management/credentials/fields"))
        .bearer_auth("fake-management-only")
        .json(&json!({"name": id, "prefix": "new"}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    assert!(f.rt.registry().ids().any(|m| m == "new/sonnet"));
    server.abort();
}

/// Update checks use exactly one credential-free HEAD, never follow redirects,
/// cache success, reject unrelated locations and honor the kill switch.
#[tokio::test]
async fn latest_version_is_anonymous_cached_and_explicit() {
    use axum::http::{HeaderMap, Method, StatusCode};
    use std::sync::Mutex;
    let seen: Arc<Mutex<Vec<(Method, HeaderMap)>>> = Arc::default();
    let record = seen.clone();
    let release = axum::Router::new().route(
        "/{case}",
        axum::routing::any(
            move |axum::extract::Path(case): axum::extract::Path<String>, method: Method, headers: HeaderMap| {
                let record = record.clone();
                async move {
                    record.lock().unwrap().push((method, headers));
                    let location = match case.as_str() {
                        "tag" => "https://github.com/vayungodara/cliproxy-rs/releases/tag/v1.2.3",
                        "other" => "https://example.invalid/releases/tag/v9",
                        "empty" => "https://github.com/vayungodara/cliproxy-rs/releases/tag/",
                        _ => "",
                    };
                    (
                        if case == "limited" {
                            StatusCode::FORBIDDEN
                        } else {
                            StatusCode::FOUND
                        },
                        [("location", location)],
                    )
                }
            },
        ),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let upstream = format!("http://{}", listener.local_addr().unwrap());
    let release_server = tokio::spawn(async move { axum::serve(listener, release).await.unwrap() });

    let get = |case: &'static str, disabled: bool| {
        let url = format!("{upstream}/{case}");
        async move {
            let f = Fixture::new(&format!("latest-{}", case));
            let options = management::Options {
                latest_release_url: Some(url),
                update_check_disabled: Some(disabled),
                ..Default::default()
            };
            let state = Management::with_options(f.rt.clone(), f.dir.join("config.yaml"), options);
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let base = format!("http://{}", listener.local_addr().unwrap());
            let app = management::router(state);
            let server = tokio::spawn(async move {
                axum::serve(
                    listener,
                    app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
                )
                .await
                .unwrap()
            });
            let mut out = (0, Value::Null);
            for _ in 0..2 {
                let r = wreq::Client::new()
                    .get(format!("{base}/v8/management/server/latest-version"))
                    .bearer_auth("fake-management-only")
                    .header("Cookie", "secret-cookie=FAKE")
                    .send()
                    .await
                    .unwrap();
                out = (r.status().as_u16(), r.json::<Value>().await.unwrap());
            }
            server.abort();
            out
        }
    };
    assert_eq!(
        get("tag", true).await,
        (
            503,
            json!({"error": "update_check_disabled", "message": "Update checks are disabled by CLIPROXY_NO_UPDATE_CHECK=1."})
        )
    );
    assert!(seen.lock().unwrap().is_empty(), "nothing is asked when disabled");
    assert_eq!(get("tag", false).await, (200, json!({"latest-version": "v1.2.3"})));
    assert_eq!(seen.lock().unwrap().len(), 1, "second click uses the cache");
    assert_eq!(
        get("empty", false).await,
        (
            502,
            json!({"error": "invalid_response", "message": "missing release version"})
        )
    );
    let (status, body) = get("other", false).await;
    assert_eq!((status, body["error"].as_str()), (502, Some("invalid_response")));
    assert_eq!(
        get("limited", false).await,
        (502, json!({"error": "unexpected_status", "message": "status 403"}))
    );
    assert!(seen.lock().unwrap().iter().all(|(method, h)| *method == Method::HEAD
        && h["user-agent"] == "cliproxy-rs"
        && !h.contains_key("authorization")
        && !h.contains_key("proxy-authorization")
        && !h.contains_key("cookie")));
    release_server.abort();
}

/// Separate processes give the environment fallback a fresh proxy snapshot without
/// changing the environment of concurrently running tests. Every address is local;
/// the unresolvable release host forces the check to use the configured transport.
#[tokio::test]
async fn latest_version_uses_configured_and_environment_proxies() {
    use axum::http::{HeaderMap, Method, Uri};
    use std::sync::Mutex;
    if let Ok(case) = std::env::var("CLIPROXY_TEST_RELEASE_PROXY") {
        let configured = std::env::var("CLIPROXY_TEST_PROXY_URL").unwrap_or_default();
        let f = Fixture::from_yaml(&format!("release-proxy-{case}"), |auth, hash| {
            format!(
                "config-version: 8\nmanagement:\n  secret-key: '{hash}'\noauth:\n  auth-dir: {}\nrequests:\n  proxy-url: '{configured}'\n",
                auth.display()
            )
        });
        let state = Management::with_options(
            f.rt.clone(),
            f.dir.join("config.yaml"),
            management::Options {
                latest_release_url: Some("http://release.example.invalid/latest".into()),
                update_check_disabled: Some(false),
                ..Default::default()
            },
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            axum::serve(
                listener,
                management::router(state).into_make_service_with_connect_info::<std::net::SocketAddr>(),
            )
            .await
            .unwrap()
        });
        let client = wreq::Client::builder().no_proxy().build().unwrap();
        for _ in 0..2 {
            let response = client
                .get(format!("{base}/v8/management/server/latest-version"))
                .bearer_auth("fake-management-only")
                .header("Cookie", "FAKE-browser-cookie")
                .send()
                .await
                .unwrap();
            if case == "direct" {
                assert_eq!(
                    response.status(),
                    502,
                    "direct must bypass the proxy for an unresolvable origin"
                );
                continue;
            }
            assert_eq!(response.status(), 200);
            assert_eq!(
                response.json::<Value>().await.unwrap(),
                json!({"latest-version": "v4.5.6"})
            );
        }
        server.abort();
        return;
    }
    for case in ["configured", "environment", "direct"] {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let record = seen.clone();
        let proxy = axum::Router::new().fallback(move |method: Method, uri: Uri, headers: HeaderMap| {
            record.lock().unwrap().push((method, uri, headers));
            async {
                (
                    axum::http::StatusCode::FOUND,
                    [(
                        "location",
                        "https://github.com/vayungodara/cliproxy-rs/releases/tag/v4.5.6",
                    )],
                )
            }
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let proxy_url = format!("http://proxy-user:proxy-password@{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move { axum::serve(listener, proxy).await.unwrap() });
        let mut command = tokio::process::Command::new(std::env::current_exe().unwrap());
        command
            .args([
                "--exact",
                "latest_version_uses_configured_and_environment_proxies",
                "--nocapture",
            ])
            .env_clear()
            .env("PATH", std::env::var("PATH").unwrap_or_default())
            .env("SYSTEMROOT", std::env::var("SYSTEMROOT").unwrap_or_default())
            .env("HOME", std::env::temp_dir())
            .env("CLIPROXY_TEST_RELEASE_PROXY", case)
            .env(
                "CLIPROXY_TEST_PROXY_URL",
                match case {
                    "configured" => &proxy_url,
                    "direct" => "direct",
                    _ => "",
                },
            )
            .env(
                "HTTP_PROXY",
                if case != "configured" {
                    &proxy_url
                } else {
                    "http://127.0.0.1:9"
                },
            )
            .env("HTTPS_PROXY", "http://127.0.0.1:9")
            .env("GITHUB_TOKEN", "FAKE-github-token")
            .env("GITSTORE_GIT_TOKEN", "FAKE-store-token")
            .env("OPENAI_API_KEY", "FAKE-provider-token")
            .kill_on_drop(true);
        let output = tokio::time::timeout(std::time::Duration::from_secs(30), command.output())
            .await
            .unwrap()
            .unwrap();
        server.abort();
        assert!(
            output.status.success(),
            "{case}: {} {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        let seen = seen.lock().unwrap();
        if case == "direct" {
            assert!(seen.is_empty(), "direct must not reach the environment proxy");
            continue;
        }
        assert_eq!(
            seen.len(),
            1,
            "two clicks share one HEAD without following the redirect"
        );
        let (method, uri, headers) = &seen[0];
        assert_eq!(*method, Method::HEAD);
        assert_eq!(uri.to_string(), "http://release.example.invalid/latest");
        assert_eq!(
            headers["proxy-authorization"],
            "Basic cHJveHktdXNlcjpwcm94eS1wYXNzd29yZA=="
        );
        assert!(!headers.contains_key("authorization"));
        assert!(!headers.contains_key("cookie"));
        assert_eq!(headers["user-agent"], "cliproxy-rs");
    }
}

/// A valid but self-signed origin certificate must still fail behind an authenticated
/// CONNECT proxy. Disabling certificate checks would turn this response into a 200.
#[tokio::test]
async fn latest_version_keeps_tls_verification_through_proxy() {
    use btls::asn1::Asn1Time;
    use btls::bn::BigNum;
    use btls::ec::{EcGroup, EcKey};
    use btls::hash::MessageDigest;
    use btls::nid::Nid;
    use btls::pkey::PKey;
    use btls::ssl::{SslAcceptor, SslMethod};
    use btls::x509::extension::SubjectAlternativeName;
    use btls::x509::{X509, X509NameBuilder};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let key =
        PKey::from_ec_key(EcKey::generate(&EcGroup::from_curve_name(Nid::X9_62_PRIME256V1).unwrap()).unwrap()).unwrap();
    let mut name = X509NameBuilder::new().unwrap();
    name.append_entry_by_nid(Nid::COMMONNAME, "127.0.0.1").unwrap();
    let name = name.build();
    let mut cert = X509::builder().unwrap();
    cert.set_version(2).unwrap();
    cert.set_serial_number(&BigNum::from_u32(1).unwrap().to_asn1_integer().unwrap())
        .unwrap();
    cert.set_subject_name(&name).unwrap();
    cert.set_issuer_name(&name).unwrap();
    cert.set_pubkey(&key).unwrap();
    cert.set_not_before(&Asn1Time::days_from_now(0).unwrap()).unwrap();
    cert.set_not_after(&Asn1Time::days_from_now(1).unwrap()).unwrap();
    cert.append_extension(
        &SubjectAlternativeName::new()
            .ip("127.0.0.1")
            .build(&cert.x509v3_context(None, None))
            .unwrap(),
    )
    .unwrap();
    cert.sign(&key, MessageDigest::sha256()).unwrap();
    let mut tls = SslAcceptor::mozilla_intermediate_v5(SslMethod::tls()).unwrap();
    tls.set_certificate(&cert.build()).unwrap();
    tls.set_private_key(&key).unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = listener.local_addr().unwrap();
    let tls_server = tokio::spawn(cpa_server::listener::serve(
        listener,
        axum::Router::new().fallback(|| async {
            (
                axum::http::StatusCode::FOUND,
                [(
                    "location",
                    "https://github.com/vayungodara/cliproxy-rs/releases/tag/v4.5.6",
                )],
            )
        }),
        Some(Arc::new(tls.build())),
    ));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let proxy = listener.local_addr().unwrap();
    let proxy_server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut head = Vec::new();
        while !head.ends_with(b"\r\n\r\n") {
            assert!(head.len() < 8192);
            head.push(socket.read_u8().await.unwrap());
        }
        socket
            .write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")
            .await
            .unwrap();
        let mut upstream = tokio::net::TcpStream::connect(origin).await.unwrap();
        let _ = tokio::io::copy_bidirectional(&mut socket, &mut upstream).await;
        String::from_utf8(head).unwrap()
    });
    let f = Fixture::from_yaml("release-proxy-tls", |auth, hash| {
        format!(
            "config-version: 8\nmanagement:\n  secret-key: '{hash}'\noauth:\n  auth-dir: {}\nrequests:\n  proxy-url: 'http://proxy-user:proxy-password@{proxy}'\n",
            auth.display()
        )
    });
    let state = Management::with_options(
        f.rt.clone(),
        f.dir.join("config.yaml"),
        management::Options {
            latest_release_url: Some(format!("https://{origin}/latest")),
            update_check_disabled: Some(false),
            ..Default::default()
        },
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        axum::serve(
            listener,
            management::router(state).into_make_service_with_connect_info::<std::net::SocketAddr>(),
        )
        .await
        .unwrap()
    });
    let response = wreq::Client::builder()
        .no_proxy()
        .build()
        .unwrap()
        .get(format!("http://{base}/v8/management/server/latest-version"))
        .bearer_auth("fake-management-only")
        .header("Cookie", "FAKE-browser-cookie")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 502);
    assert_eq!(response.json::<Value>().await.unwrap()["error"], "request_failed");
    let head = tokio::time::timeout(std::time::Duration::from_secs(2), proxy_server)
        .await
        .unwrap()
        .unwrap()
        .to_ascii_lowercase();
    assert!(head.starts_with(&format!("connect {origin} http/1.1\r\n")));
    assert!(
        head.contains(&"\r\nProxy-Authorization: Basic cHJveHktdXNlcjpwcm94eS1wYXNzd29yZA==\r\n".to_ascii_lowercase())
    );
    assert!(!head.contains("\r\nauthorization:") && !head.contains("\r\ncookie:"));
    server.abort();
    tls_server.abort();
}

/// AI Studio relay credentials: disabling one through the status or fields endpoint
/// clears its cooldowns (Go `Manager.Update`), so re-enabling routes to it at once.
#[tokio::test]
async fn runtime_credential_disable_clears_its_cooldowns() {
    use cpa_core::credential::Credential;
    use cpa_core::exec::{ExecError, FailureScope};
    use cpa_server::runtime::{Outcome, Selection};
    let f = Fixture::new("runtime-cooldown");
    let id = "aistudio-0123456789abcdef";
    assert!(f.rt.store().add_runtime(Credential::relay_session(id)));
    let selection = || Selection {
        provider: "aistudio".into(),
        ..Selection::default()
    };
    let (base, server) = f.server().await;
    let client = wreq::Client::new();
    for endpoint in ["status", "fields"] {
        let lease = f.rt.store().select(selection()).unwrap();
        assert_eq!(lease.credential.id, id);
        let mut quota = ExecError::local(429, FailureScope::Credential, "rate limited");
        quota.retry_after = Some(std::time::Duration::from_secs(600));
        lease.complete(Outcome::Failure(quota));
        assert!(!f.rt.store().cooldowns(id).is_empty(), "{endpoint}: cooling");
        assert!(f.rt.store().select(selection()).is_none(), "{endpoint}: blocked");
        for disabled in [true, false] {
            let r = client
                .patch(format!("{base}/v8/management/credentials/{endpoint}"))
                .bearer_auth("fake-management-only")
                .json(&json!({"name": id, "disabled": disabled}))
                .send()
                .await
                .unwrap();
            assert_eq!(r.status(), 200, "{endpoint} disabled={disabled}");
            assert_eq!(f.rt.store().get(id).unwrap().disabled, disabled);
            assert!(f.rt.store().cooldowns(id).is_empty(), "{endpoint} disabled={disabled}");
        }
        let lease = f.rt.store().select(selection());
        assert_eq!(
            lease.map(|l| l.credential.id.clone()).as_deref(),
            Some(id),
            "{endpoint}"
        );
    }
    server.abort();
}

/// Deleting an AI Studio relay credential (no file) answers 404 and never removes a
/// file in `auth-dir` that shares its ID.
#[tokio::test]
async fn runtime_credential_delete_spares_same_named_file() {
    let f = Fixture::new("runtime-delete");
    let id = "aistudio-0123456789abcdef";
    assert!(
        f.rt.store()
            .add_runtime(cpa_core::credential::Credential::relay_session(id))
    );
    let sentinel = f.dir.join("auth").join(id);
    std::fs::write(&sentinel, "keep").unwrap();
    let (base, server) = f.server().await;
    let r = wreq::Client::new()
        .delete(format!("{base}/v8/management/credentials?name={id}"))
        .bearer_auth("fake-management-only")
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 404);
    assert_eq!(r.json::<Value>().await.unwrap()["error"], "auth file not found");
    assert_eq!(std::fs::read_to_string(&sentinel).unwrap(), "keep");
    assert!(f.rt.store().get(id).is_some());
    server.abort();
}

/// `GET /observability/usage/queue` drains queued records oldest first; a record that
/// is not JSON comes back as a string. Disabling statistics stops queueing.
#[tokio::test]
async fn usage_queue_pops_records_in_order() {
    let f = Fixture::from_yaml("usageq", |auth, hash| {
        format!(
            "config-version: 8\nmanagement:\n  secret-key: '{hash}'\noauth:\n  auth-dir: {}\nobservability:\n  usage:\n    usage-statistics-enabled: true\n",
            auth.display()
        )
    });
    let (base, server) = f.server().await;
    let queue = f.rt.usage_queue();
    queue.enqueue(br#"{"model":"a","tokens":{"total_tokens":3}}"#.to_vec());
    queue.enqueue(b"not json".to_vec());
    queue.enqueue(br#"{"model":"c"}"#.to_vec());
    let client = wreq::Client::new();
    let pop = |q: &str| {
        client
            .get(format!("{base}/v8/management/observability/usage/queue{q}"))
            .bearer_auth("fake-management-only")
            .send()
    };
    let first: Value = pop("").await.unwrap().json().await.unwrap();
    assert_eq!(first, json!([{"model": "a", "tokens": {"total_tokens": 3}}]));
    let rest: Value = pop("?count=10").await.unwrap().json().await.unwrap();
    assert_eq!(rest, json!(["not json", {"model": "c"}]));
    let r = client
        .put(format!(
            "{base}/v8/management/config/observability/usage/usage-statistics-enabled"
        ))
        .bearer_auth("fake-management-only")
        .body("false")
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    assert!(!queue.accepts());
    queue.enqueue(br#"{"model":"d"}"#.to_vec());
    let none: Value = pop("?count=10").await.unwrap().json().await.unwrap();
    assert_eq!(none, json!([]));
    server.abort();
}

/// api-call HEAD: Go's transport never asks for gzip on HEAD, so the upstream sees no
/// Accept-Encoding and a gzip Content-Encoding on the answer is relayed untouched.
#[tokio::test]
async fn api_call_head_does_not_negotiate_gzip() {
    use axum::http::{HeaderMap, HeaderValue};
    let upstream = axum::Router::new().route(
        "/h",
        axum::routing::head(|headers: HeaderMap| async move {
            let ae = headers
                .get("accept-encoding")
                .cloned()
                .unwrap_or_else(|| HeaderValue::from_static("none"));
            (
                [("x-ae", ae), ("content-encoding", HeaderValue::from_static("gzip"))],
                "",
            )
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/h", listener.local_addr().unwrap());
    let up = tokio::spawn(async move { axum::serve(listener, upstream).await.unwrap() });
    let f = Fixture::new("apihead");
    let (base, server) = f.server().await;
    let r: Value = wreq::Client::new()
        .post(format!("{base}/v8/management/requests/api-call"))
        .bearer_auth("fake-management-only")
        .json(&json!({"method": "HEAD", "url": url}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(r["status_code"], 200);
    assert_eq!(r["header"]["X-Ae"], json!(["none"]));
    assert_eq!(r["header"]["Content-Encoding"], json!(["gzip"]));
    assert_eq!(r["body"], "");
    server.abort();
    up.abort();
}

/// Fake provider login endpoints (Claude, Codex, Kimi) on one local server; records
/// the form or JSON bodies it receives.
async fn fake_logins() -> (
    String,
    Arc<std::sync::Mutex<Vec<(String, String)>>>,
    tokio::task::JoinHandle<()>,
) {
    use base64::Engine;
    let seen: Arc<std::sync::Mutex<Vec<(String, String)>>> = Arc::default();
    let record = seen.clone();
    let b64 = |v: &Value| base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(v.to_string());
    let id_token = format!(
        "{}.{}.sig",
        b64(&json!({"alg": "none"})),
        b64(&json!({"email": "c@example.invalid",
            "https://api.openai.com/auth": {"chatgpt_plan_type": "plus", "chatgpt_account_id": "acc-fake"}}))
    );
    let xai_id_token = format!(
        "{}.{}.sig",
        b64(&json!({"alg": "none"})),
        b64(&json!({"email": "x@example.invalid", "sub": "xai-user-1"}))
    );
    let app = axum::Router::new().fallback(move |uri: axum::http::Uri, body: axum::body::Bytes| {
        let record = record.clone();
        let id_token = id_token.clone();
        let xai_id_token = xai_id_token.clone();
        async move {
            record
                .lock()
                .unwrap()
                .push((uri.path().to_owned(), String::from_utf8_lossy(&body).into_owned()));
            let v = match uri.path() {
                "/oauth/token" => json!({"id_token": id_token, "access_token": "fake-codex-at",
                    "refresh_token": "fake-codex-rt", "expires_in": 3600}),
                "/v1/oauth/token" => json!({"access_token": "sk-ant-oat-fake", "refresh_token": "fake-r",
                    "expires_in": 3600, "account": {"uuid": "acct-1", "email_address": "a@example.invalid"},
                    "organization": {"uuid": "org-1", "name": "Org"}}),
                "/api/oauth/profile" => json!({"account": {"uuid": "acct-1", "email": "a@example.invalid"},
                    "organization": {"uuid": "org-1", "name": "Org"}}),
                "/api/oauth/device_authorization" => json!({"device_code": "dc", "user_code": "UC-1",
                    "verification_uri": "https://kimi.example.invalid/device",
                    "verification_uri_complete": "https://kimi.example.invalid/device?code=UC-1",
                    "expires_in": 600, "interval": 1}),
                "/api/oauth/token" => json!({"access_token": "fake-kimi-at", "refresh_token": "fake-kimi-rt",
                    "token_type": "Bearer", "expires_in": 3600, "scope": "s"}),
                // xAI: discovery names auth.x.ai endpoints, which the test seam redirects here.
                "/.well-known/openid-configuration" => json!({
                    "device_authorization_endpoint": "https://auth.x.ai/oauth2/device/code",
                    "token_endpoint": "https://auth.x.ai/oauth2/token"}),
                "/oauth2/device/code" => json!({"device_code": "xdc", "user_code": "XU-1",
                    "verification_uri": "https://accounts.x.ai/device",
                    "verification_uri_complete": "https://accounts.x.ai/device?user_code=XU-1",
                    "expires_in": 600, "interval": 1}),
                "/oauth2/token" => json!({"access_token": "fake-xai-at", "refresh_token": "fake-xai-rt",
                    "id_token": xai_id_token, "token_type": "Bearer", "expires_in": 3600}),
                // Devin: code exchange and profile (the user-status RPC gets `{}` and fails,
                // which the login tolerates).
                "/auth/cli/token" => json!({"token": "fake-devin-session"}),
                "/v3/self" => json!({"user_name": "dev-user", "user_id": "u-1", "org_id": "o-1"}),
                _ => json!({}),
            };
            axum::Json(v)
        }
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let handle = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (base, seen, handle)
}

/// Serves `state` like `Fixture::server` but with login endpoints redirected.
async fn login_server(f: &Fixture, login_base: &str) -> (String, Arc<Management>, tokio::task::JoinHandle<()>) {
    let options = management::Options {
        login_base: Some(login_base.to_owned()),
        ..Default::default()
    };
    let state = Management::with_options(f.rt.clone(), f.dir.join("config.yaml"), options);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let app = management::router(state.clone());
    let handle = tokio::spawn(async move {
        axum::serve(
            listener,
            app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
        )
        .await
        .unwrap()
    });
    (base, state, handle)
}

async fn oauth_get(base: &str, path: &str) -> Value {
    wreq::Client::new()
        .get(format!("{base}/v8/management{path}"))
        .bearer_auth("fake-management-only")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap()
}

/// Polls `/oauth/status` until it leaves `wait` (at most `secs`).
async fn final_status(base: &str, state: &str, secs: u64) -> Value {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(secs);
    loop {
        let v = oauth_get(base, &format!("/oauth/status?state={state}")).await;
        if v["status"] != "wait" || std::time::Instant::now() > deadline {
            return v;
        }
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    }
}

/// xAI: Go `RequestXAIToken` answers with the device URL and code, and the poller
/// saves the credential through the file store and publishes it.
#[tokio::test]
async fn xai_login_polls_the_device_flow_and_saves_the_credential() {
    let (fake, seen, fake_server) = fake_logins().await;
    let f = Fixture::new("xailogin");
    let (base, _state, server) = login_server(&f, &fake).await;
    let started = oauth_get(&base, "/oauth/auth-url?provider=xai").await;
    let state = started["state"].as_str().unwrap().to_owned();
    assert!(state.starts_with("xai-"), "{state}");
    assert_eq!(started["status"], "ok");
    assert_eq!(started["flow"], "device");
    assert_eq!(started["url"], "https://accounts.x.ai/device?user_code=XU-1");
    assert_eq!(started["user_code"], "XU-1");
    assert_eq!(started["expires_in"], 600);
    assert_eq!(final_status(&base, &state, 10).await, json!({"status": "ok"}));
    let auth = f.dir.join("auth");
    let names: Vec<String> = std::fs::read_dir(&auth)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|n| n.starts_with("xai-"))
        .collect();
    assert_eq!(names.len(), 1, "{names:?}");
    let path = auth.join(&names[0]);
    let saved: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    for (key, want) in [
        ("type", json!("xai")),
        ("access_token", json!("fake-xai-at")),
        ("refresh_token", json!("fake-xai-rt")),
        ("email", json!("x@example.invalid")),
        ("sub", json!("xai-user-1")),
        ("base_url", json!("https://api.x.ai/v1")),
        ("token_endpoint", json!("https://auth.x.ai/oauth2/token")),
        ("auth_kind", json!("oauth")),
        ("expires_in", json!(3600)),
        ("disabled", json!(false)),
    ] {
        assert_eq!(saved[key], want, "{key}");
    }
    let last_refresh = saved["last_refresh"].as_str().unwrap();
    assert!(chrono::DateTime::parse_from_rfc3339(last_refresh).is_ok() && last_refresh.ends_with('Z'));
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
    }
    let forms = seen.lock().unwrap().clone();
    let device = forms.iter().find(|(p, _)| p == "/oauth2/device/code").unwrap();
    assert!(
        device.1.contains("client_id=b1a00492-073a-47ea-816f-4c329264a828"),
        "{}",
        device.1
    );
    let token = forms.iter().find(|(p, _)| p == "/oauth2/token").unwrap();
    assert!(token.1.contains("device_code=xdc"), "{}", token.1);
    assert!(f.rt.store().snapshot().iter().any(|c| c.provider == "xai"), "published");
    server.abort();
    fake_server.abort();
}

/// Devin: Go `RequestDevinToken` needs the listener port for its fixed loopback
/// redirect; the main listener's `/callback` completes the login.
#[tokio::test]
async fn devin_login_completes_through_the_main_listener_callback() {
    let (fake, seen, fake_server) = fake_logins().await;
    // Port 0: no redirect URI can be built.
    let f = Fixture::new("devinlogin-noport");
    let (base, _state, server) = login_server(&f, &fake).await;
    let r = oauth_get(&base, "/oauth/auth-url?provider=devin").await;
    assert_eq!(r, json!({"error": "callback server unavailable"}));
    server.abort();

    let f = Fixture::from_yaml("devinlogin", |auth, hash| {
        format!(
            "config-version: 8\nserver:\n  port: 18999\nmanagement:\n  secret-key: '{hash}'\noauth:\n  auth-dir: {}\n",
            auth.display()
        )
    });
    let (base, _state, server) = login_server(&f, &fake).await;
    let deliver = |state: &str, code: &str, error: &str| {
        f.rt.deliver_oauth_callback(&cpa_server::runtime::OAuthCallback {
            provider: "devin",
            state: state.to_owned(),
            code: code.to_owned(),
            error: error.to_owned(),
        })
    };
    // A denied authorization ends the session with Go's message.
    let started = oauth_get(&base, "/oauth/auth-url?provider=devin").await;
    let denied = started["state"].as_str().unwrap().to_owned();
    assert!(deliver(&denied, "", "access_denied"));
    assert_eq!(
        final_status(&base, &denied, 5).await,
        json!({"status": "error", "error": "Devin authorization denied"})
    );
    // A callback for another provider or an unknown state is not delivered.
    let started = oauth_get(&base, "/oauth/auth-url?provider=devin").await;
    assert_eq!(started["status"], "ok");
    let state = started["state"].as_str().unwrap().to_owned();
    assert_eq!(state.len(), 32, "Go misc.GenerateRandomState");
    let url = url::Url::parse(started["url"].as_str().unwrap()).unwrap();
    let query: std::collections::HashMap<String, String> = url.query_pairs().into_owned().collect();
    assert_eq!(query["redirect_uri"], "http://127.0.0.1:18999/callback");
    assert_eq!(query["state"], state);
    assert!(!deliver("unknown-state", "c", ""));
    assert!(!f.rt.deliver_oauth_callback(&cpa_server::runtime::OAuthCallback {
        provider: "codex",
        state: state.clone(),
        code: "c".into(),
        error: String::new(),
    }));
    assert!(deliver(&state, "fake-devin-code", ""));
    assert_eq!(final_status(&base, &state, 10).await, json!({"status": "ok"}));
    let path = f.dir.join("auth/devin-dev-user.json");
    let saved: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    for (key, want) in [
        ("type", "devin"),
        ("api_key", "fake-devin-session"),
        ("session_token", "fake-devin-session"),
        ("user_name", "dev-user"),
        ("user_id", "u-1"),
        ("org_id", "o-1"),
        ("auth_kind", "oauth"),
    ] {
        assert_eq!(saved[key], want, "{key}");
    }
    assert_eq!(saved["disabled"], false);
    let exchange = seen
        .lock()
        .unwrap()
        .iter()
        .find(|(p, _)| p == "/auth/cli/token")
        .unwrap()
        .1
        .clone();
    let exchange: Value = serde_json::from_str(&exchange).unwrap();
    assert_eq!(exchange["code"], "fake-devin-code");
    let verifier = exchange["code_verifier"].as_str().unwrap();
    // The challenge sent to the browser is S256(verifier).
    use base64::Engine;
    use sha2::Digest;
    let challenge = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(sha2::Sha256::digest(verifier.as_bytes()));
    assert_eq!(query["code_challenge"], challenge);
    assert!(f.rt.store().get("devin-dev-user.json").is_some(), "published");
    server.abort();
    fake_server.abort();
}

/// Codex: the main listener's `/codex/callback` hands the code to the pending login,
/// which exchanges it (PKCE verifier, Go's redirect URI) and saves the credential.
#[tokio::test]
async fn codex_login_completes_through_the_main_listener_callback() {
    let (fake, seen, fake_server) = fake_logins().await;
    let f = Fixture::new("codexlogin");
    let (base, _state, server) = login_server(&f, &fake).await;
    let started = oauth_get(&base, "/oauth/auth-url?provider=codex").await;
    let state = started["state"].as_str().unwrap().to_owned();
    assert!(
        started["url"]
            .as_str()
            .unwrap()
            .starts_with("https://auth.openai.com/oauth/authorize?")
    );
    let delivered = f.rt.deliver_oauth_callback(&cpa_server::runtime::OAuthCallback {
        provider: "codex",
        state: state.clone(),
        code: "fake-code".into(),
        error: String::new(),
    });
    assert!(delivered);
    assert_eq!(final_status(&base, &state, 10).await, json!({"status": "ok"}));
    let token_request = seen
        .lock()
        .unwrap()
        .iter()
        .find(|(p, _)| p == "/oauth/token")
        .unwrap()
        .1
        .clone();
    assert!(token_request.contains("code=fake-code"), "{token_request}");
    assert!(
        token_request.contains("redirect_uri=http%3A%2F%2Flocalhost%3A1455%2Fauth%2Fcallback"),
        "{token_request}"
    );
    assert!(token_request.contains("code_verifier="), "{token_request}");
    let creds = f.rt.store().snapshot();
    let codex = creds
        .iter()
        .find(|c| c.provider == "codex")
        .expect("saved and published");
    assert_eq!(codex.str("email"), Some("c@example.invalid"));
    assert!(!f.rt.deliver_oauth_callback(&cpa_server::runtime::OAuthCallback {
        provider: "codex",
        state,
        code: "again".into(),
        error: String::new(),
    }));
    server.abort();
    fake_server.abort();
}

/// Claude: a pasted callback URL posted to `/oauth/callback` finishes the login.
#[tokio::test]
async fn claude_login_completes_from_a_posted_redirect_url() {
    let (fake, _seen, fake_server) = fake_logins().await;
    let f = Fixture::new("claudelogin");
    let (base, _state, server) = login_server(&f, &fake).await;
    let started = oauth_get(&base, "/oauth/auth-url?provider=claude").await;
    let state = started["state"].as_str().unwrap().to_owned();
    let r: Value = wreq::Client::new()
        .post(format!("{base}/v8/management/oauth/callback"))
        .json(&json!({"provider": "claude",
            "redirect_url": format!("http://localhost:54545/callback?code=fake-code&state={state}")}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(r, json!({"status": "ok"}), "the callback needs no management key");
    assert_eq!(final_status(&base, &state, 10).await, json!({"status": "ok"}));
    let creds = f.rt.store().snapshot();
    let claude = creds
        .iter()
        .find(|c| c.provider == "claude")
        .expect("saved and published");
    assert_eq!(claude.str("access_token"), Some("sk-ant-oat-fake"));
    server.abort();
    fake_server.abort();
}

/// Kimi device login saves after authorization; a cancelled one saves nothing.
#[tokio::test]
async fn kimi_device_login_saves_and_a_cancelled_one_does_not() {
    let (fake, _seen, fake_server) = fake_logins().await;
    let f = Fixture::new("kimilogin");
    let (base, _state, server) = login_server(&f, &fake).await;
    let started = oauth_get(&base, "/oauth/auth-url?provider=kimi").await;
    assert_eq!(started["flow"], "device");
    assert_eq!(started["user_code"], "UC-1");
    assert_eq!(started["expires_in"], 600);
    assert_eq!(started["url"], "https://kimi.example.invalid/device?code=UC-1");
    let state = started["state"].as_str().unwrap().to_owned();
    assert!(state.starts_with("kmi-"));
    let cancelled = oauth_get(&base, "/oauth/auth-url?provider=kimi-ai").await;
    let other = cancelled["state"].as_str().unwrap().to_owned();
    let r: Value = wreq::Client::new()
        .delete(format!("{base}/v8/management/oauth/session?state={other}"))
        .bearer_auth("fake-management-only")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(r, json!({"status": "ok", "cancelled": true}));
    assert_eq!(final_status(&base, &state, 15).await, json!({"status": "ok"}));
    tokio::time::sleep(std::time::Duration::from_secs(3)).await;
    let names: Vec<String> = std::fs::read_dir(f.dir.join("auth"))
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(
        names
            .iter()
            .filter(|n| n.starts_with("kimi-") && !n.starts_with("kimi-ai"))
            .count(),
        1,
        "{names:?}"
    );
    assert!(!names.iter().any(|n| n.starts_with("kimi-ai")), "{names:?}");
    server.abort();
    fake_server.abort();
}

/// `provider=kimi` honours Go's `domain`/`channel` selectors, and a failed Codex
/// exchange reports Go's "token exchange failed" text in the session.
#[tokio::test]
async fn kimi_channel_selects_kimi_ai_and_codex_exchange_errors_read_like_go() {
    let (fake, _seen, fake_server) = fake_logins().await;
    let f = Fixture::new("loginselect");
    let (base, _state, server) = login_server(&f, &fake).await;
    let started = oauth_get(&base, "/oauth/auth-url?provider=kimi&channel=ai").await;
    let state = started["state"].as_str().unwrap().to_owned();
    assert!(state.starts_with("kmi-ai-"), "{state}");
    let _ = wreq::Client::new()
        .delete(format!("{base}/v8/management/oauth/session?state={state}"))
        .bearer_auth("fake-management-only")
        .send()
        .await
        .unwrap();
    server.abort();

    // A token endpoint that rejects the code.
    let rejecting = axum::Router::new()
        .fallback(|| async { (axum::http::StatusCode::BAD_REQUEST, r#"{"error":"invalid_grant"}"#) });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let bad = format!("http://{}", listener.local_addr().unwrap());
    let bad_server = tokio::spawn(async move { axum::serve(listener, rejecting).await.unwrap() });
    let (base, _state, server) = login_server(&f, &bad).await;
    let started = oauth_get(&base, "/oauth/auth-url?provider=codex").await;
    let state = started["state"].as_str().unwrap().to_owned();
    assert!(f.rt.deliver_oauth_callback(&cpa_server::runtime::OAuthCallback {
        provider: "codex",
        state: state.clone(),
        code: "fake-code".into(),
        error: String::new(),
    }));
    // Go appends the upstream body; session errors are readable by anyone holding
    // the state, so cliproxy-rs stops at the status (deliberate hardening).
    assert_eq!(
        final_status(&base, &state, 10).await,
        json!({"status": "error", "error":
            "Failed to exchange authorization code for tokens: token exchange failed with status 400"})
    );
    server.abort();
    bad_server.abort();
    fake_server.abort();
}
