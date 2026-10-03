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

async fn serve(path: &std::path::Path) -> (String, tokio::task::JoinHandle<()>) {
    let config = Config::load(path).unwrap();
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
        std::fs::write(&path, scenario["yaml"].as_str().unwrap().replace("$HASH", &hash)).unwrap();
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
            let res = send(method, route, step["body"].as_str().unwrap_or_default().to_owned())
                .await
                .unwrap();
            let status = res.status().as_u16();
            let body = res.text().await.unwrap();
            compared += 1;
            if status != step["status"].as_u64().unwrap() as u16 {
                failures.push(format!("{at}: status {status}, Go {}: {body}", step["status"]));
                continue;
            }
            let want = step["raw_response"].as_str().unwrap();
            if let Some(d) = first_difference(&body, want) {
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
    assert!(compared >= 762, "{compared}");
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
    std::fs::write(&path, yaml.replace("$HASH", &hash)).unwrap();
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
        let creds = cpa_core::config::credentials::load(&config);
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
    use std::os::unix::fs::PermissionsExt;
    let hash = bcrypt::hash("fake-secret", 4).unwrap();
    let dir = std::env::temp_dir().join(format!("cpa-legacy-save-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("config.yaml");
    std::fs::write(
        &path,
        format!(
            "config-version: 8\nmanagement:\n  secret-key: '{hash}'\napi-keys:\n  claude:\n    # keep this comment\n    - name: c\n      base-url: https://c.example.invalid\n      keys: [{{api-key: fake-1}}, {{api-key: fake-2}}]\n"
        ),
    )
    .unwrap();
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

    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o400)).unwrap();
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o500)).unwrap();
    // Root ignores the modes; there is nothing to check then.
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
    }
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    server.abort();
    let _ = std::fs::remove_dir_all(&dir);
}
