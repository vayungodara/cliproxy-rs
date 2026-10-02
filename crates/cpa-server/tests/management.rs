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
                codex: Default::default(),
                devices: Default::default(),
                openai: Default::default(),
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
        let rt = Arc::new(Runtime::new(
            cfg.clone(),
            cpa_core::config::credentials::load(&cfg),
            Executors {
                claude: ClaudeExecutor::new("http://127.0.0.1:1").unwrap(),
                codex: Default::default(),
                devices: Default::default(),
                openai: Default::default(),
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

/// `GET /server/latest-version`: Go's failed-lookup shape while no release
/// repository is configured, and Go's handling of each release-API answer.
#[tokio::test]
async fn latest_version_follows_go_for_each_release_answer() {
    use axum::http::{HeaderMap, StatusCode};
    use std::sync::Mutex;
    let seen: Arc<Mutex<Vec<(String, String)>>> = Arc::default();
    let record = seen.clone();
    let release = axum::Router::new().route(
        "/{case}",
        axum::routing::get(
            move |axum::extract::Path(case): axum::extract::Path<String>, headers: HeaderMap| {
                let record = record.clone();
                async move {
                    let h = |n: &str| {
                        headers
                            .get(n)
                            .and_then(|v| v.to_str().ok())
                            .unwrap_or_default()
                            .to_owned()
                    };
                    record.lock().unwrap().push((h("accept"), h("user-agent")));
                    match case.as_str() {
                        "tag" => (StatusCode::OK, r#"{"tag_name":" v1.2.3 ","name":"ignored"}"#.to_owned()),
                        "name" => (StatusCode::OK, r#"{"tag_name":"","name":"Release 9"}"#.to_owned()),
                        "empty" => (StatusCode::OK, r#"{"tag_name":" "}"#.to_owned()),
                        "bad" => (StatusCode::OK, "not json".to_owned()),
                        _ => (StatusCode::FORBIDDEN, " rate limited \n".to_owned()),
                    }
                }
            },
        ),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let upstream = format!("http://{}", listener.local_addr().unwrap());
    let release_server = tokio::spawn(async move { axum::serve(listener, release).await.unwrap() });

    let get = |url: Option<String>| async move {
        let f = Fixture::new(&format!(
            "latest-{}",
            url.as_deref().map_or("none", |u| u.rsplit('/').next().unwrap())
        ));
        let options = management::Options {
            latest_release_url: url,
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
        let r = wreq::Client::new()
            .get(format!("{base}/v8/management/server/latest-version"))
            .bearer_auth("fake-management-only")
            .send()
            .await
            .unwrap();
        let out = (r.status().as_u16(), r.json::<Value>().await.unwrap());
        server.abort();
        out
    };
    assert_eq!(
        get(None).await,
        (
            502,
            json!({"error": "request_failed", "message": "no release repository is configured"})
        )
    );
    assert!(seen.lock().unwrap().is_empty(), "nothing is asked while unconfigured");
    assert_eq!(
        get(Some(format!("{upstream}/tag"))).await,
        (200, json!({"latest-version": "v1.2.3"}))
    );
    assert_eq!(
        get(Some(format!("{upstream}/name"))).await,
        (200, json!({"latest-version": "Release 9"}))
    );
    assert_eq!(
        get(Some(format!("{upstream}/empty"))).await,
        (
            502,
            json!({"error": "invalid_response", "message": "missing release version"})
        )
    );
    let (status, body) = get(Some(format!("{upstream}/bad"))).await;
    assert_eq!((status, body["error"].as_str()), (502, Some("decode_failed")));
    assert_eq!(
        get(Some(format!("{upstream}/limited"))).await,
        (
            502,
            json!({"error": "unexpected_status", "message": "status 403: rate limited"})
        )
    );
    assert!(
        seen.lock()
            .unwrap()
            .iter()
            .all(|h| *h == ("application/vnd.github+json".to_owned(), "cliproxy-rs".to_owned()))
    );
    release_server.abort();
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
    let app = axum::Router::new().fallback(move |uri: axum::http::Uri, body: axum::body::Bytes| {
        let record = record.clone();
        let id_token = id_token.clone();
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
    assert_eq!(
        final_status(&base, &state, 10).await,
        json!({"status": "error", "error":
            "Failed to exchange authorization code for tokens: token exchange failed with status 400: {\"error\":\"invalid_grant\"}"})
    );
    server.abort();
    bad_server.abort();
    fake_server.abort();
}
