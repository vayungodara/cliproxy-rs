//! Replays expectations recorded from the pinned Go reference
//! (tests/reference/manage/main.go -> tests/fixtures/manage_go.json).
use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::path::Path;

use cpa_core::config::{Config, TrustedProxies, credentials};
use cpa_core::credential::{Credential, Source};
use serde_json::{Value, json};

fn fixture() -> &'static Value {
    static FIXTURE: std::sync::LazyLock<Value> = std::sync::LazyLock::new(|| {
        serde_json::from_str(include_str!("fixtures/manage_go.json")).expect("fixture JSON")
    });
    &FIXTURE
}

fn strings(v: &Value) -> Vec<String> {
    v.as_array()
        .map(|a| a.iter().map(|s| s.as_str().unwrap().to_owned()).collect())
        .unwrap_or_default()
}

fn first_header<'a>(headers: &'a Value, name: &str) -> Option<&'a [u8]> {
    headers
        .as_object()?
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(name))?
        .1
        .as_array()?
        .first()?
        .as_str()
        .map(str::as_bytes)
}

#[test]
fn client_ip_matches_gin_for_every_trusted_remote_and_header_combination() {
    let cases = fixture()["client_ip"].as_array().unwrap();
    assert!(cases.len() > 300);
    for case in cases {
        let trusted = TrustedProxies::new(&strings(&case["trusted"]));
        let remote: SocketAddr = case["remote"].as_str().unwrap().parse().unwrap();
        let got = trusted.client_ip(Some(remote), |name| first_header(&case["headers"], name));
        assert_eq!(got, case["ip"].as_str().unwrap(), "{case}");
    }
}

fn unhex(text: &str) -> Vec<u8> {
    (0..text.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&text[i..i + 2], 16).unwrap())
        .collect()
}

#[test]
fn forwarded_header_bytes_and_zoned_peers_match_gin() {
    let cases = fixture()["client_ip_bytes"].as_array().unwrap();
    assert_eq!(cases.len(), 10);
    for case in cases {
        let trusted = TrustedProxies::new(&strings(&case["trusted"]));
        let remote = case["remote"].as_str().unwrap();
        // Go's zone text has no Rust parse form; a nonzero scope is the same fact.
        let peer: SocketAddr = match remote.split_once('%') {
            Some((ip, _)) => std::net::SocketAddrV6::new(ip.trim_start_matches('[').parse().unwrap(), 1, 0, 2).into(),
            None => remote.parse().unwrap(),
        };
        let xff = unhex(case["xff_hex"].as_str().unwrap());
        let real = case["x_real_ip_hex"].as_str().map(unhex);
        let got = trusted.client_ip(Some(peer), |name| match name {
            "X-Forwarded-For" => Some(xff.as_slice()),
            "X-Real-IP" => real.as_deref(),
            _ => None,
        });
        assert_eq!(got, case["ip"].as_str().unwrap(), "{case}");
    }
}

#[test]
fn trusted_proxy_validation_errors_match_go_text() {
    for case in fixture()["load_errors"].as_array().unwrap() {
        let yaml = case["yaml"].as_str().unwrap();
        let got = Config::parse(yaml).err().map(|e| e.to_string()).unwrap_or_default();
        assert_eq!(got, case["error"].as_str().unwrap(), "{yaml}");
    }
}

/// Go's shape: prefix/proxy_url are Auth fields, hashes are not ported, and the
/// scheduler-only metadata copies are Rust additions on config-backed credentials.
fn go_shape(c: &Credential, root: &Path) -> Value {
    let mut attrs = c.attributes.clone();
    let prefix = attrs.remove("prefix").unwrap_or_default();
    let proxy = attrs.remove("proxy_url").unwrap_or_default();
    let mut meta = c.metadata.clone();
    let mut c = c.clone();
    if let Source::File(path) = &c.source {
        let rel = path.strip_prefix(root).unwrap();
        let placeholder = Path::new("/fixture-root").join(rel);
        for key in ["path", "source"] {
            attrs.insert(key.into(), placeholder.display().to_string());
        }
        c.source = Source::File(placeholder);
    } else {
        meta.remove("excluded_models");
        meta.remove("model_aliases");
    }
    c.attributes = attrs.clone();
    json!({
        "id": c.id, "provider": c.provider, "label": c.label, "prefix": prefix, "proxy_url": proxy,
        "disabled": c.disabled, "auth_index": credentials::auth_index(&c),
        "attributes": attrs, "metadata": meta,
    })
}

fn go_expected(v: &Value) -> Value {
    let mut attrs: BTreeMap<String, Value> = serde_json::from_value(v["attributes"].clone()).unwrap();
    attrs.remove("models_hash");
    attrs.remove("excluded_models_hash");
    attrs.remove("file_name");
    json!({
        "id": v["id"], "provider": v["provider"], "label": v["label"], "prefix": v["prefix"],
        "proxy_url": v["proxy_url"], "disabled": v["disabled"], "auth_index": v["auth_index"],
        "attributes": attrs, "metadata": v["metadata"],
    })
}

#[test]
fn config_and_file_credentials_match_go_synthesizers() {
    let mut compared = 0;
    for case in fixture()["synth"].as_array().unwrap() {
        let name = case["name"].as_str().unwrap();
        let dir = std::env::temp_dir().join(format!("cpa-synth-{name}-{}", std::process::id()));
        let root = dir.join("fixture-root");
        std::fs::create_dir_all(root.join("auth")).unwrap();
        let path = root.join("config.yaml");
        std::fs::write(&path, case["yaml"].as_str().unwrap()).unwrap();
        for (file, body) in case["files"].as_object().into_iter().flatten() {
            std::fs::write(root.join("auth").join(file), body.as_str().unwrap()).unwrap();
        }
        let loaded = Config::load(&path);
        if case["error"].is_string() {
            assert!(loaded.is_err(), "{name}: Go rejected this config");
            continue;
        }
        let mut cfg = loaded.unwrap();
        cfg.auth_dir = root.join("auth");
        let got: Vec<Value> = credentials::from_config(&cfg)
            .iter()
            .map(|c| go_shape(c, &root))
            .collect();
        let want: Vec<Value> = case["config"].as_array().unwrap().iter().map(go_expected).collect();
        assert_eq!(got, want, "{name}: config credentials");
        compared += got.len();
        let got: Vec<Value> = credentials::from_auth_dir(&cfg)
            .iter()
            .map(|c| go_shape(c, &root))
            .collect();
        let want: Vec<Value> = case["file"].as_array().unwrap().iter().map(go_expected).collect();
        assert_eq!(got, want, "{name}: file credentials");
        compared += got.len();
        let _ = std::fs::remove_dir_all(&dir);
    }
    assert_eq!(compared, 19, "every recorded Go credential was compared");
}

mod access {
    use super::*;
    use cpa_exec::Executors;
    use cpa_exec::claude::ClaudeExecutor;
    use cpa_server::Runtime;
    use cpa_server::management::{self, Management, Options};
    use std::sync::Arc;

    /// Serves the real management router; `X-Test-Peer` stands in for the socket peer.
    pub(super) async fn serve(state: Arc<Management>) -> (String, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let app = management::router(state).layer(axum::middleware::from_fn(
            |mut req: axum::extract::Request, next: axum::middleware::Next| async move {
                let peer: SocketAddr = req.headers()["x-test-peer"].to_str().unwrap().parse().unwrap();
                req.extensions_mut().insert(axum::extract::ConnectInfo(peer));
                next.run(req).await
            },
        ));
        (
            base,
            tokio::spawn(async move { axum::serve(listener, app).await.unwrap() }),
        )
    }

    fn state(dir: &Path, scenario: &Value) -> Arc<Management> {
        std::fs::create_dir_all(dir).unwrap();
        let path = dir.join("config.yaml");
        let mut yaml = format!(
            "config-version: 8\nmanagement:\n  allow-remote: {}\n",
            scenario["allow_remote"]
        );
        if let Some(secret) = scenario["secret"].as_str() {
            yaml += &format!("  secret-key: '{}'\n", bcrypt::hash(secret, 4).unwrap());
        }
        if let Some(t) = scenario["trusted"].as_array() {
            yaml += &format!("server:\n  trusted-proxies: {}\n", Value::Array(t.clone()));
        }
        std::fs::write(&path, yaml).unwrap();
        let rt = Arc::new(Runtime::new(
            Config::load(&path).unwrap(),
            vec![],
            Executors {
                claude: ClaudeExecutor::new("http://127.0.0.1:1").unwrap(),
                codex: Default::default(),
                devices: Default::default(),
                openai: Default::default(),
            },
        ));
        let options = Options {
            local_password: scenario["local_password"].as_str().unwrap_or_default().into(),
            management_password: Some(scenario["env"].as_str().unwrap_or_default().into()),
        };
        Management::with_options(rt, path, options)
    }

    #[tokio::test]
    async fn every_go_access_scenario_replays_through_the_router() {
        let client = wreq::Client::new();
        let mut steps = 0;
        for scenario in fixture()["access"].as_array().unwrap() {
            let name = scenario["name"].as_str().unwrap();
            let dir = std::env::temp_dir().join(format!("cpa-access-{name}-{}", std::process::id()));
            let (base, server) = serve(state(&dir, scenario)).await;
            for (i, step) in scenario["steps"].as_array().unwrap().iter().enumerate() {
                // HTTP parsers (Go's textproto and hyper) trim header whitespace, so
                // httptest-only values are covered by unit tests instead.
                let headers = step["headers"].as_object();
                if headers.into_iter().flatten().any(|(_, vs)| {
                    vs.as_array()
                        .unwrap()
                        .iter()
                        .any(|v| v.as_str().unwrap().ends_with(' '))
                }) {
                    continue;
                }
                let mut req = client
                    .get(format!("{base}/v8/management/config"))
                    .header("X-Test-Peer", step["remote"].as_str().unwrap());
                for (k, values) in step["headers"].as_object().into_iter().flatten() {
                    for v in values.as_array().unwrap() {
                        // Raw bytes, so non-ASCII values travel as obs-text like Go's.
                        let value = wreq::header::HeaderValue::from_bytes(v.as_str().unwrap().as_bytes()).unwrap();
                        req = req.header(k.as_str(), value);
                    }
                }
                let res = req.send().await.unwrap();
                let status = res.status().as_u16();
                let version = res.headers().contains_key("x-cpa-version");
                let body = res.text().await.unwrap();
                assert_eq!(status, step["status"], "{name}[{i}] {body}");
                if status != 200 {
                    assert_eq!(body, step["body"].as_str().unwrap(), "{name}[{i}]");
                }
                assert_eq!(version, step["x_cpa_version"] != "", "{name}[{i}] version header");
                steps += 1;
            }
            server.abort();
            let _ = std::fs::remove_dir_all(&dir);
        }
        assert_eq!(steps, 70, "71 recorded steps, one not wire-representable");
    }
}

mod routes {
    use super::*;
    use cpa_exec::Executors;
    use cpa_exec::claude::ClaudeExecutor;
    use cpa_server::management::{Management, Options};
    use cpa_server::{Runtime, watching};
    use std::sync::Arc;

    #[tokio::test]
    async fn go_server_routing_preflight_and_availability_replay() {
        let hash = bcrypt::hash("fake-secret", 4).unwrap();
        let client = wreq::Client::builder()
            .redirect(wreq::redirect::Policy::none())
            .build()
            .unwrap();
        let mut compared = 0;
        for scenario in fixture()["routes"].as_array().unwrap() {
            let name = scenario["name"].as_str().unwrap();
            let dir = std::env::temp_dir().join(format!("cpa-routes-{name}-{}", std::process::id()));
            std::fs::create_dir_all(&dir).unwrap();
            let path = dir.join("config.yaml");
            let write = |yaml: &str| std::fs::write(&path, yaml.replace("$HASH", &hash)).unwrap();
            write(scenario["yaml"].as_str().unwrap());
            let rt = Arc::new(Runtime::new(
                Config::load(&path).unwrap(),
                vec![],
                Executors {
                    claude: ClaudeExecutor::new("http://127.0.0.1:1").unwrap(),
                    codex: Default::default(),
                    devices: Default::default(),
                    openai: Default::default(),
                },
            ));
            let state = Management::with_options(
                rt,
                path.clone(),
                Options {
                    local_password: scenario["local_password"].as_str().unwrap_or_default().into(),
                    management_password: Some(scenario["env"].as_str().unwrap_or_default().into()),
                },
            );
            let (base, server) = super::access::serve(state.clone()).await;
            for (i, step) in scenario["steps"].as_array().unwrap().iter().enumerate() {
                if let Some(update) = step["update"].as_str() {
                    write(update);
                    watching::reload(&state).unwrap();
                }
                if step["status"] == 307 {
                    continue; // gin trailing-slash redirect: not reproduced (documented gap)
                }
                let method: wreq::Method = step["method"].as_str().unwrap().parse().unwrap();
                let mut req = client
                    .request(method, format!("{base}{}", step["path"].as_str().unwrap()))
                    .header("X-Test-Peer", step["remote"].as_str().unwrap());
                for (k, values) in step["headers"].as_object().into_iter().flatten() {
                    for v in values.as_array().unwrap() {
                        req = req.header(k.as_str(), v.as_str().unwrap());
                    }
                }
                let res = req.send().await.unwrap();
                let status = res.status().as_u16();
                let mut headers = serde_json::Map::new();
                for name in [
                    "Content-Type",
                    "Cache-Control",
                    "Access-Control-Allow-Origin",
                    "Access-Control-Allow-Methods",
                    "Access-Control-Allow-Headers",
                    "Access-Control-Expose-Headers",
                    "X-CPA-SUPPORT-PLUGIN",
                    "X-CPA-COMMIT",
                    "X-CPA-BUILD-DATE",
                ] {
                    if let Some(v) = res.headers().get(name) {
                        headers.insert(name.into(), v.to_str().unwrap().into());
                    }
                }
                if res.headers().contains_key("x-cpa-version") {
                    headers.insert("X-CPA-VERSION".into(), "present".into());
                }
                assert!(!res.headers().contains_key("allow"), "{name}[{i}]: gin sends no Allow");
                let body = res.text().await.unwrap();
                assert_eq!(status, step["status"], "{name}[{i}] {body}");
                if status != 200 {
                    assert_eq!(body, step["resp_body"].as_str().unwrap(), "{name}[{i}]");
                }
                let mut want = step["resp_headers"].as_object().unwrap().clone();
                // Go's cgo build can load native plugins; this binary cannot.
                if want.contains_key("X-CPA-SUPPORT-PLUGIN") {
                    want.insert("X-CPA-SUPPORT-PLUGIN".into(), "0".into());
                }
                assert_eq!(Value::Object(headers), Value::Object(want), "{name}[{i}] headers");
                compared += 1;
            }
            server.abort();
            let _ = std::fs::remove_dir_all(&dir);
        }
        assert_eq!(compared, 24, "25 recorded steps, minus one trailing-slash redirect");
    }
}
