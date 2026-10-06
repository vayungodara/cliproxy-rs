//! Replays Go's v0 management routes (tests/reference/legacy/main.go ->
//! tests/fixtures/legacy_go.json) against the Rust router: same status, same body
//! byte for byte (Go field order included), and after every write the same
//! GET /v0/management/config.
use std::net::SocketAddr;
use std::sync::Arc;

use cpa_core::config::Config;
use cpa_exec::Executors;
use cpa_exec::claude::ClaudeExecutor;
use cpa_server::management::{self, Management, Options};
use serde_json::Value;

fn fixture() -> Value {
    serde_json::from_str(include_str!("fixtures/legacy_go.json")).expect("fixture JSON")
}

/// Go's harness registers no credentials, so its lists carry no `auth-index`; the
/// Rust store synthesizes config credentials. Indexes are covered separately.
fn without_auth_index(v: &mut Value) {
    match v {
        Value::Object(m) => {
            m.shift_remove("auth-index");
            m.values_mut().for_each(without_auth_index);
        }
        Value::Array(a) => a.iter_mut().for_each(without_auth_index),
        _ => {}
    }
}

/// The first path where two JSON bodies differ, with both values (or the texts).
fn first_difference(rust: &str, go: &str) -> Option<String> {
    fn walk(r: &Value, g: &Value, at: String) -> Option<String> {
        match (r, g) {
            (Value::Object(a), Value::Object(b)) => {
                let (ka, kb): (Vec<_>, Vec<_>) = (a.keys().collect(), b.keys().collect());
                for (k, gv) in b {
                    match a.get(k) {
                        Some(rv) => {
                            if let Some(d) = walk(rv, gv, format!("{at}.{k}")) {
                                return Some(d);
                            }
                        }
                        None => return Some(format!("{at}.{k}: missing in Rust, Go {gv}")),
                    }
                }
                if let Some(k) = ka.iter().find(|k| !b.contains_key(**k)) {
                    return Some(format!("{at}.{k}: only in Rust: {}", a[*k]));
                }
                (ka != kb).then(|| format!("{at}: key order {ka:?} vs Go {kb:?}"))
            }
            (Value::Array(a), Value::Array(b)) if a.len() == b.len() => a
                .iter()
                .zip(b)
                .enumerate()
                .find_map(|(i, (x, y))| walk(x, y, format!("{at}[{i}]"))),
            _ if r == g => None,
            _ => Some(format!("{at}: Rust {r} vs Go {g}")),
        }
    }
    if rust == go {
        return None;
    }
    let (r, g) = (normalized(rust), normalized(go));
    if r == g {
        // Bodies with live auth indexes are compared without them; every other body
        // must match byte for byte (gin's HTML escaping included).
        return (!rust.contains("\"auth-index\""))
            .then(|| format!("same JSON, other bytes: Rust {rust:?} vs Go {go:?}"));
    }
    match (serde_json::from_str(&r), serde_json::from_str(&g)) {
        (Ok(a), Ok(b)) => walk(&a, &b, "$".into()).or(Some("serialization differs".into())),
        _ => Some(format!("Rust {r:?} vs Go {g:?}")),
    }
}

fn normalized(body: &str) -> String {
    match serde_json::from_str::<Value>(body) {
        Ok(mut v) => {
            without_auth_index(&mut v);
            v.to_string()
        }
        Err(_) => body.to_owned(),
    }
}

/// Group names per v8 key family. Go saves a nil list as `family: []`; the shared
/// writer removes the family instead (both read back as no keys), so an empty list
/// and an absent family compare equal.
fn group_difference(saved: &Value, go: &serde_json::Map<String, Value>) -> Option<String> {
    let names = |groups: Option<&Value>| -> Vec<String> {
        groups
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .map(|g| g.as_str().or(g["name"].as_str()).unwrap_or_default().to_owned())
            .collect()
    };
    let families: std::collections::BTreeSet<&String> = go
        .keys()
        .chain(saved.as_object().into_iter().flat_map(|o| o.keys()))
        .collect();
    families.into_iter().find_map(|family| {
        let (rust, go) = (names(saved.get(family)), names(go.get(family)));
        (rust != go).then(|| format!("{family}: Rust {rust:?} vs Go {go:?}"))
    })
}

/// Gives a test config its own auth directory (the legacy top-level `auth-dir`, which
/// both layouts accept) unless it names one, so no test reads Go's default
/// `~/.cli-proxy-api`. Go's JSON config view omits `auth-dir`, so replies are unchanged.
fn with_auth_dir(yaml: &str, dir: &std::path::Path) -> String {
    if yaml.lines().any(|l| l.trim_start().starts_with("auth-dir:")) {
        return yaml.to_owned();
    }
    let auth = dir.join("auth");
    std::fs::create_dir_all(&auth).unwrap();
    let mut out = yaml.to_owned();
    if !out.is_empty() && !out.ends_with('\n') {
        out.push('\n');
    }
    out.push_str(&format!("auth-dir: '{}'\n", auth.display()));
    out
}

async fn serve(path: &std::path::Path) -> (String, tokio::task::JoinHandle<()>) {
    // A private auth-dir, so a config without one never reads ~/.cli-proxy-api.
    let auth = path.parent().unwrap().join("auth");
    let mut config = Config::load(path).unwrap();
    config.auth_dir = auth.clone();
    // As main.rs starts the server: config keys are credentials from the start.
    let credentials = cpa_core::config::credentials::load(&config);
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
    let state = Management::with_options(
        rt,
        path.to_path_buf(),
        Options {
            management_password: Some(String::new()),
            auth_dir: Some(auth),
            // Sign-in flows that start a device login hit this closed port, never a provider.
            login_base: Some("http://127.0.0.1:1".into()),
            ..Options::default()
        },
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let app = management::router(state).layer(axum::middleware::from_fn(
        |mut req: axum::extract::Request, next: axum::middleware::Next| async move {
            req.extensions_mut()
                .insert(axum::extract::ConnectInfo(SocketAddr::from(([127, 0, 0, 1], 1))));
            next.run(req).await
        },
    ));
    (
        base,
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() }),
    )
}

#[tokio::test]
async fn go_v0_routes_replay_byte_for_byte() {
    let hash = bcrypt::hash("fake-secret", 4).unwrap();
    let client = wreq::Client::new();
    let mut failures: Vec<String> = Vec::new();
    let mut compared = 0;
    for scenario in fixture()["scenarios"].as_array().unwrap() {
        let name = scenario["name"].as_str().unwrap();
        let dir = std::env::temp_dir().join(format!("cpa-legacy-{name}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.yaml");
        let yaml = scenario["yaml"].as_str().unwrap().replace("$HASH", &hash);
        std::fs::write(&path, with_auth_dir(&yaml, &dir)).unwrap();
        let (base, server) = serve(&path).await;
        let send = |method: &str, path: &str, body: String| {
            let method: wreq::Method = method.parse().unwrap();
            client
                .request(method, format!("{base}/v0/management{path}"))
                .bearer_auth("fake-secret")
                .body(body)
                .send()
        };
        for (i, step) in scenario["steps"].as_array().unwrap().iter().enumerate() {
            let (method, route) = (step["method"].as_str().unwrap(), step["path"].as_str().unwrap());
            let at = format!(
                "{name}[{i}] {method} {route} {}",
                step["body"].as_str().unwrap_or_default()
            );
            // An accepted config.yaml upload replaces the whole file and is reloaded, so it
            // keeps the private auth directory too.
            let mut sent = step["body"].as_str().unwrap_or_default().to_owned();
            if method == "PUT" && route == "/config.yaml" && step["status"] == 200 && !sent.is_empty() {
                sent = with_auth_dir(&sent, &dir);
            }
            let res = send(method, route, sent).await.unwrap();
            let status = res.status().as_u16();
            let body = res.text().await.unwrap();
            compared += 1;
            if status != step["status"].as_u64().unwrap() as u16 {
                failures.push(format!("{at}: status {status}, Go {}: {body}", step["status"]));
                continue;
            }
            let want = step["raw_response"].as_str().unwrap();
            // yaml.v3's own syntax messages ("yaml: line 1: ...") cannot match another
            // parser; for those only the status and the error code are compared.
            let go_body: Value = serde_json::from_str(want).unwrap_or_default();
            if go_body["message"].as_str().is_some_and(|m| m.starts_with("yaml: ")) {
                let rust_body: Value = serde_json::from_str(&body).unwrap_or_default();
                if rust_body["error"] != go_body["error"] {
                    failures.push(format!(
                        "{at}: error code {} vs Go {}",
                        rust_body["error"], go_body["error"]
                    ));
                }
            } else if let Some(d) = first_difference(&body, want) {
                failures.push(format!("{at}: {d}"));
            }
            if let Some(go_cfg) = step["config"].as_str() {
                let res = send("GET", "/config", String::new()).await.unwrap();
                let got = res.text().await.unwrap();
                if let Some(d) = first_difference(&got, go_cfg) {
                    failures.push(format!("{at}: config after write: {d}"));
                }
            }
            if let Some(go_groups) = step["groups"].as_object() {
                let res = client
                    .get(format!("{base}/v8/management/config/api-keys"))
                    .bearer_auth("fake-secret")
                    .send()
                    .await
                    .unwrap();
                let saved: Value = res.json().await.unwrap_or_default();
                if let Some(d) = group_difference(&saved, go_groups) {
                    failures.push(format!("{at}: v8 groups after write: {d}"));
                }
            }
        }
        server.abort();
        let _ = std::fs::remove_dir_all(&dir);
    }
    assert!(compared >= 816, "{compared}");
    assert!(
        failures.is_empty(),
        "{} of {compared} steps differ:\n{}",
        failures.len(),
        failures.join("\n")
    );
}

/// v0 aliases must answer exactly like the v8 routes they wrap, and list entries carry
/// the same `auth_index` the credentials listing reports for that key.
#[tokio::test]
async fn v0_aliases_match_v8_and_lists_carry_live_auth_indexes() {
    let hash = bcrypt::hash("fake-secret", 4).unwrap();
    let dir = std::env::temp_dir().join(format!("cpa-legacy-alias-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("config.yaml");
    let yaml = "config-version: 8\nmanagement:\n  secret-key: '$HASH'\napi-keys:\n  gemini:\n    - name: g\n      keys: [{api-key: fake-g1}, {api-key: fake-g2}]\n  vertex:\n    - name: v\n      keys: [{api-key: fake-v1}]\n  openai-compatibility:\n    - name: Local\n      base-url: http://127.0.0.1:9/v1\n      keys: [{api-key: fake-o1}]\n";
    std::fs::write(&path, with_auth_dir(&yaml.replace("$HASH", &hash), &dir)).unwrap();
    let (base, server) = serve(&path).await;
    let client = wreq::Client::new();
    let get = |method: &str, path: &str| {
        client
            .request(method.parse::<wreq::Method>().unwrap(), format!("{base}{path}"))
            .bearer_auth("fake-secret")
            .send()
    };
    for (v0, v8) in [
        (
            "/v0/management/antigravity-auth-url",
            "/v8/management/oauth/auth-url?provider=antigravity",
        ),
        (
            "/v0/management/xai-auth-url?is_webui=true",
            "/v8/management/oauth/auth-url?provider=xai&is_webui=true",
        ),
        (
            "/v0/management/devin-auth-url",
            "/v8/management/oauth/auth-url?provider=devin",
        ),
        ("/v0/management/logs", "/v8/management/observability/logs"),
        (
            "/v0/management/request-error-logs",
            "/v8/management/observability/logs/errors",
        ),
        (
            "/v0/management/request-log-by-id/nope",
            "/v8/management/observability/logs/requests/nope",
        ),
    ] {
        let (a, b) = (get("GET", v0).await.unwrap(), get("GET", v8).await.unwrap());
        assert_eq!(a.status(), b.status(), "{v0}");
        assert_eq!(a.text().await.unwrap(), b.text().await.unwrap(), "{v0}");
    }
    let (a, b) = (
        get("POST", "/v0/management/vertex/import").await.unwrap(),
        get("POST", "/v8/management/oauth/import?provider=vertex")
            .await
            .unwrap(),
    );
    assert_eq!(
        (a.status(), a.text().await.unwrap()),
        (b.status(), b.text().await.unwrap())
    );

    let lists: Value = get("GET", "/v0/management/gemini-api-key")
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let entries = lists["gemini-api-key"].as_array().unwrap();
    assert_eq!(entries.len(), 2);
    let config = Config::load(&path).unwrap();
    let expected = |key: &str| {
        let creds = cpa_core::config::credentials::from_config(&config);
        let c = creds
            .iter()
            .find(|c| c.attributes.get("api_key").map(String::as_str) == Some(key))
            .unwrap_or_else(|| panic!("credential for {key}"));
        cpa_core::config::credentials::auth_index(c)
    };
    for (entry, key) in entries.iter().zip(["fake-g1", "fake-g2"]) {
        assert_eq!(entry["auth-index"].as_str(), Some(expected(key).as_str()), "{entry}");
    }
    let vertex: Value = get("GET", "/v0/management/vertex-api-key")
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(
        vertex["vertex-api-key"][0]["auth-index"].as_str(),
        Some(expected("fake-v1").as_str())
    );
    let compat: Value = get("GET", "/v0/management/openai-compatibility")
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let entry = &compat["openai-compatibility"][0];
    assert_eq!(entry["disabled"], false);
    assert_eq!(
        entry["api-key-entries"][0]["auth-index"].as_str(),
        Some(expected("fake-o1").as_str()),
        "{compat}"
    );
    server.abort();
    let _ = std::fs::remove_dir_all(&dir);
}

/// Go's `persistLocked` saves after every write handler, even one that changed
/// nothing, and answers a failed save with `failed to save config: <error>`. An
/// unchanged family keeps its v8 groups and their comments.
#[tokio::test]
async fn v0_saves_like_go_persist_locked() {
    let hash = bcrypt::hash("fake-secret", 4).unwrap();
    let dir = std::env::temp_dir().join(format!("cpa-legacy-save-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("config.yaml");
    let yaml = format!(
        "config-version: 8\nmanagement:\n  secret-key: '{hash}'\napi-keys:\n  claude:\n    # keep this comment\n    - name: c\n      base-url: https://c.example.invalid\n      keys: [{{api-key: fake-1}}, {{api-key: fake-2}}]\n"
    );
    std::fs::write(&path, with_auth_dir(&yaml, &dir)).unwrap();
    let (base, server) = serve(&path).await;
    let client = wreq::Client::new();
    let send = |method: &str, route: &str, body: &str| {
        client
            .request(method.parse().unwrap(), format!("{base}/v0/management{route}"))
            .bearer_auth("fake-secret")
            .body(body.to_owned())
            .send()
    };
    let res = send("DELETE", "/claude-api-key?api-key=missing", "").await.unwrap();
    assert_eq!(res.status().as_u16(), 200);
    assert_eq!(res.text().await.unwrap(), r#"{"status":"ok"}"#);
    let saved = std::fs::read_to_string(&path).unwrap();
    assert!(
        saved.contains("# keep this comment") && saved.contains("name: c"),
        "{saved}"
    );

    // The shared writer rewrites the file in place, so a read-only file fails the save
    // (on Unix and Windows alike).
    let writable = std::fs::metadata(&path).unwrap().permissions();
    let mut read_only = writable.clone();
    read_only.set_readonly(true);
    std::fs::set_permissions(&path, read_only).unwrap();
    // Root ignores the mode; there is nothing to check then.
    if std::fs::OpenOptions::new().append(true).open(&path).is_err() {
        for (method, route, body) in [
            ("PATCH", "/claude-api-key", r#"{"index":0,"value":{}}"#),
            ("PATCH", "/claude-api-key", r#"{"index":0,"value":{"priority":3}}"#),
            ("PUT", "/debug", r#"{"value":true}"#),
        ] {
            let res = send(method, route, body).await.unwrap();
            assert_eq!(res.status().as_u16(), 500, "{method} {route} {body}");
            let text = res.text().await.unwrap();
            assert!(text.starts_with(r#"{"error":"failed to save config: "#), "{text}");
        }
    } else {
        // CI sets CPA_TEST_NO_SKIP: a run as root must not pass without the check.
        assert!(
            std::env::var_os("CPA_TEST_NO_SKIP").is_none(),
            "file permissions are not enforced (running as root?), and CPA_TEST_NO_SKIP is set"
        );
    }
    std::fs::set_permissions(&path, writable).unwrap();
    server.abort();
    let _ = std::fs::remove_dir_all(&dir);
}

/// v0 PUT /config.yaml beyond the replay: the upload is written as sent with comment
/// lines unindented and a plaintext key hashed on reload (Go `WriteConfig` then
/// `LoadConfig`); with no management key left, the routes go away as Go's reload
/// disables them.
#[tokio::test]
async fn v0_config_yaml_put_writes_like_go() {
    let hash = bcrypt::hash("fake-secret", 4).unwrap();
    let dir = std::env::temp_dir().join(format!("cpa-legacy-yaml-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("config.yaml");
    let yaml = format!("config-version: 8\nmanagement:\n  secret-key: '{hash}'\n");
    std::fs::write(&path, with_auth_dir(&yaml, &dir)).unwrap();
    let (base, server) = serve(&path).await;
    let client = wreq::Client::new();
    let put = |body: &str| {
        client
            .put(format!("{base}/v0/management/config.yaml"))
            .bearer_auth("fake-secret")
            .body(body.to_owned())
            .send()
    };
    let upload = "config-version: 8\nmanagement:\n  secret-key: fake-secret\n    # indented comment\nrouting:\n  strategy: fill-first\n";
    let res = put(&with_auth_dir(upload, &dir)).await.unwrap();
    assert_eq!(res.status().as_u16(), 200);
    assert_eq!(res.text().await.unwrap(), r#"{"changed":["config"],"ok":true}"#);
    let saved = std::fs::read_to_string(&path).unwrap();
    assert!(saved.contains("\n# indented comment\n"), "{saved}");
    // A `#` line inside a block scalar is content; unindenting it ends the scalar and
    // leaves `second` dangling. The upload is valid as sent, so the check must run on
    // the unindented text, and the file must stay as it was.
    let block = with_auth_dir(
        "config-version: 8\nmanagement:\n  secret-key: fake-secret\nnote: |\n  # first\n  second\n",
        &dir,
    );
    serde_yaml_ng::from_str::<serde_yaml_ng::Value>(&block).expect("valid as sent");
    let res = put(&block).await.unwrap();
    assert_eq!(res.status().as_u16(), 400);
    assert!(res.text().await.unwrap().starts_with(r#"{"error":"invalid_yaml""#));
    assert_eq!(std::fs::read_to_string(&path).unwrap(), saved);
    assert!(saved.contains("strategy: fill-first"), "{saved}");
    assert!(!saved.contains("secret-key: fake-secret"), "the key is hashed: {saved}");
    // The hashed key still authenticates.
    let res = client
        .get(format!("{base}/v0/management/routing/strategy"))
        .bearer_auth("fake-secret")
        .send()
        .await
        .unwrap();
    assert_eq!(res.text().await.unwrap(), r#"{"strategy":"fill-first"}"#);
    // A config without a management key is accepted (Go also accepts an empty upload,
    // which would reload with the default auth directory).
    let keyless = with_auth_dir("", &dir);
    let res = put(&keyless).await.unwrap();
    assert_eq!(res.status().as_u16(), 200);
    assert_eq!(res.text().await.unwrap(), r#"{"changed":["config"],"ok":true}"#);
    assert_eq!(std::fs::read_to_string(&path).unwrap(), keyless);
    let res = client
        .get(format!("{base}/v0/management/debug"))
        .bearer_auth("fake-secret")
        .send()
        .await
        .unwrap();
    assert_eq!(res.status().as_u16(), 404);
    server.abort();
    let _ = std::fs::remove_dir_all(&dir);
}
