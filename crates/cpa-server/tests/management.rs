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
impl Fixture {
    fn new(name: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("manage-{name}-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("auth")).unwrap();
        let path = dir.join("config.yaml");
        let hash = bcrypt::hash("fake-management-only", 4).unwrap();
        std::fs::write(&path, format!("# operator note\nconfig-version: 8\nserver:\n  host: '127.0.0.1' # listener\n  port: 0\nmanagement:\n  secret-key: '{hash}'\noauth:\n  auth-dir: {}\nrouting:\n  retry:\n    request-retry: 3 # attempts\naccess:\n  api-keys: [fake-client]\n", dir.join("auth").display())).unwrap();
        let cfg = Config::load(&path).unwrap();
        let rt = Arc::new(Runtime::new(
            cfg,
            vec![],
            Executors {
                claude: ClaudeExecutor::new("http://127.0.0.1:1").unwrap(),
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
