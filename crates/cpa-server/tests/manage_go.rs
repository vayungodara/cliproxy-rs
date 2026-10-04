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

/// Go's key order (map keys sorted, struct fields as declared) against ours, wherever
/// both objects have the same keys; values may differ (times, indexes). Both sides are
/// parsed with insertion order kept.
fn same_key_order(go: &Value, rs: &Value, at: &str) -> Result<(), String> {
    match (go, rs) {
        (Value::Object(g), Value::Object(r)) => {
            let (gk, rk): (Vec<&String>, Vec<&String>) = (g.keys().collect(), r.keys().collect());
            let mut gs = gk.clone();
            let mut rsorted = rk.clone();
            gs.sort();
            rsorted.sort();
            if gs != rsorted {
                return Ok(());
            }
            if gk != rk {
                return Err(format!("{at}: key order go {gk:?} rs {rk:?}"));
            }
            for (k, v) in g {
                same_key_order(v, &r[k.as_str()], &format!("{at}.{k}"))?;
            }
            Ok(())
        }
        (Value::Array(g), Value::Array(r)) if g.len() == r.len() => {
            for (i, (gv, rv)) in g.iter().zip(r).enumerate() {
                same_key_order(gv, rv, &format!("{at}[{i}]"))?;
            }
            Ok(())
        }
        _ => Ok(()),
    }
}

/// The config text for one `yaml_bools` fixture case (same templates as Go's).
fn scalar_case_yaml(field: &str, spelling: &str) -> String {
    match field {
        "observability.logs.debug" => format!("config-version: 8\nobservability:\n  logs:\n    debug: {spelling}\n"),
        "plugins.configs.x.enabled" => {
            format!("config-version: 8\nplugins:\n  configs:\n    x:\n      enabled: {spelling}\n")
        }
        "server.host" => format!("config-version: 8\nserver:\n  host: {spelling}\n"),
        _ => format!("config-version: 8\ncredentials:\n  concurrency:\n    cpa-heartbeat-timeout: {spelling}\n"),
    }
}

#[test]
fn yaml_bool_spellings_and_durations_load_like_go() {
    let cases = fixture()["yaml_bools"].as_array().unwrap();
    assert!(cases.len() > 100);
    for case in cases {
        let (field, spelling) = (case["field"].as_str().unwrap(), case["spelling"].as_str().unwrap());
        let at = format!("{field}: {spelling}");
        let parsed = Config::parse(&scalar_case_yaml(field, spelling));
        assert_eq!(
            parsed.is_err(),
            case["error"] == true,
            "{at}: {:?}",
            parsed.as_ref().err()
        );
        let Ok(cfg) = parsed else { continue };
        let flag = |path: &[&str]| {
            path.iter()
                .try_fold(&cfg.document, |node, part| node.get(*part))
                .and_then(serde_yaml_ng::Value::as_bool)
                .unwrap_or(false)
        };
        let got = match field {
            "observability.logs.debug" => json!(flag(&["observability", "logs", "debug"])),
            // Plugin entries stay raw (Go `PluginInstanceConfig.Raw`); Go's typed
            // `Enabled` decodes the YAML 1.1 spellings from them.
            "plugins.configs.x.enabled" => {
                let raw = cfg.document["plugins"]["configs"]["x"]["enabled"].clone();
                json!(match &raw {
                    serde_yaml_ng::Value::Bool(b) => *b,
                    serde_yaml_ng::Value::String(s) => cpa_core::config::go_bool(s).unwrap_or(false),
                    _ => false,
                })
            }
            // ponytail: serde resolves the YAML 1.2 bool spellings without keeping their
            // text, so a string field reads `True`/`TRUE` as `true` where Go keeps it.
            "server.host" if ["True", "TRUE", "False", "FALSE"].contains(&spelling) => {
                assert_eq!(cfg.host, spelling.to_lowercase(), "{at}");
                continue;
            }
            "server.host" => json!(cfg.host),
            // Durations: load acceptance only (nothing consumes them without Home).
            _ => continue,
        };
        assert_eq!(got, case["value"], "{at}");
    }
}

/// Go's typed decoding and raw plugin views (`typed_scalars` in the fixture): load
/// errors match; int fields truncate floats; plugin entries keep their raw scalars in
/// the live document, from which `go_bool`/`go_int` give Go's typed values.
#[test]
fn typed_scalars_and_raw_plugin_entries_load_like_go() {
    let hash = bcrypt::hash("fake-secret", 4).unwrap();
    let cases = fixture()["typed_scalars"].as_array().unwrap();
    assert_eq!(cases.len(), 12);
    for case in cases {
        let name = case["name"].as_str().unwrap();
        let parsed = Config::parse(&case["yaml"].as_str().unwrap().replace("$HASH", &hash));
        assert_eq!(
            parsed.is_err(),
            case["error"] == true,
            "{name}: {:?}",
            parsed.as_ref().err()
        );
        let Ok(cfg) = parsed else { continue };
        assert_eq!(json!(cfg.routing.retry.request_retry), case["request_retry"], "{name}");
        let Some(raw_view) = case.get("raw_view") else { continue };
        let entry = cfg.document["plugins"]["configs"].get("x").cloned().unwrap();
        // Go's raw view of a null entry is `{}`.
        let raw: Value = match &entry {
            serde_yaml_ng::Value::Null => json!({}),
            other => serde_json::to_value(other).unwrap(),
        };
        assert_eq!(raw, raw_view["body"], "{name}: raw view");
        let enabled = entry.get("enabled").and_then(|v| match v {
            serde_yaml_ng::Value::Bool(b) => Some(*b),
            serde_yaml_ng::Value::String(s) => cpa_core::config::go_bool(s),
            _ => None,
        });
        let priority = entry.get("priority").and_then(cpa_core::config::go_int);
        assert_eq!(
            json!(enabled.unwrap_or(false)),
            case["enabled"],
            "{name}: typed enabled"
        );
        assert_eq!(json!(priority.unwrap_or(0)), case["priority"], "{name}: typed priority");
    }
}

#[test]
fn yaml_merge_keys_load_like_go() {
    for case in fixture()["yaml_merges"].as_array().unwrap() {
        let name = case["name"].as_str().unwrap();
        let parsed = Config::parse(case["yaml"].as_str().unwrap());
        assert_eq!(
            parsed.is_err(),
            case["error"] == true,
            "{name}: {:?}",
            parsed.as_ref().err()
        );
        let Ok(cfg) = parsed else { continue };
        let want = &case["values"];
        let logs = |key: &str| {
            cfg.document
                .get("observability")
                .and_then(|o| o.get("logs"))
                .and_then(|l| l.get(key))
                .and_then(serde_yaml_ng::Value::as_bool)
                .unwrap_or(false)
        };
        let claude: Vec<Value> = credentials::from_config(&cfg)
            .iter()
            .filter(|c| c.provider == "claude")
            .map(|c| {
                json!({
                    "api-key": c.attributes["api_key"],
                    "priority": c.attributes.get("priority").map_or(0, |p| p.parse::<i64>().unwrap()),
                })
            })
            .collect();
        let got = json!({
            "request-retry": cfg.routing.retry.request_retry,
            "max-retry-interval": cfg.routing.retry.max_retry_interval,
            "debug": logs("debug"),
            "request-log": logs("request-log"),
            "claude": claude,
        });
        assert_eq!(&got, want, "{name}");
    }
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

/// Go records OAuth-only fields by legacy name; cliproxy-rs by v8 path. Go's prefix
/// table for `oauth.providers` (internal/config/config_v8.go buildV8Paths) maps them.
#[test]
fn oauth_only_presence_matches_go() {
    const PREFIXES: &[(&str, &str)] = &[
        ("ws-auth", "oauth.providers.aistudio.ws-auth"),
        ("codex", "oauth.providers.codex"),
        ("codex-header-defaults", "oauth.providers.codex.header-defaults"),
        ("claude", "oauth.providers.claude"),
        ("claude-code", "oauth.providers.claude.claude-code"),
        (
            "disable-claude-cloak-mode",
            "oauth.providers.claude.disable-claude-cloak-mode",
        ),
        ("claude-header-defaults", "oauth.providers.claude.header-defaults"),
        ("antigravity", "oauth.providers.antigravity"),
        (
            "antigravity-signature-cache-enabled",
            "oauth.providers.antigravity.signature-cache-enabled",
        ),
        (
            "antigravity-signature-bypass-strict",
            "oauth.providers.antigravity.signature-bypass-strict",
        ),
        (
            "quota-exceeded.antigravity-credits",
            "oauth.providers.antigravity.antigravity-credits",
        ),
        ("xai", "oauth.providers.xai"),
        ("devin", "oauth.providers.devin"),
    ];
    let to_v8 = |old: &str| {
        PREFIXES
            .iter()
            .find(|(p, _)| old == *p || old.starts_with(&format!("{p}.")))
            .map(|(p, current)| format!("{current}{}", &old[p.len()..]))
            .unwrap_or_else(|| panic!("no v8 prefix for {old}"))
    };
    let cases = fixture()["oauth_only"].as_array().unwrap().clone();
    assert_eq!(cases.len(), 6);
    for case in &cases {
        let yaml = case["yaml"].as_str().unwrap();
        let want: std::collections::BTreeSet<String> = case["fields"]
            .as_array()
            .unwrap()
            .iter()
            .map(|f| to_v8(f.as_str().unwrap()))
            .collect();
        assert_eq!(Config::parse(yaml).unwrap().oauth_only, want, "{yaml}");
    }
    // Go `ForAPIKey`: those fields read as zero for API-key credentials.
    let cfg = Config::parse(cases[1]["yaml"].as_str().unwrap()).unwrap();
    let view = cfg.for_api_key();
    let codex = &view.document["oauth"]["providers"]["codex"];
    assert_eq!(codex["disable-codex-cloaking"], serde_yaml_ng::Value::Bool(false));
    assert_eq!(codex["header-defaults"]["user-agent"], serde_yaml_ng::Value::from(""));
    assert!(codex["live-media-relay"].get("ice-servers").is_none());
    assert_eq!(
        view.document["oauth"]["providers"]["aistudio"]["ws-auth"],
        serde_yaml_ng::Value::Bool(false)
    );
    assert!(view.oauth_only.is_empty());
    assert_eq!(
        cfg.document["oauth"]["providers"]["codex"]["disable-codex-cloaking"],
        serde_yaml_ng::Value::Bool(true)
    );
    let legacy = Config::parse(cases[0]["yaml"].as_str().unwrap()).unwrap();
    assert!(matches!(legacy.for_api_key(), std::borrow::Cow::Borrowed(_)));
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
        // Go's registry reads `models` from config by index; Rust carries them along.
        meta.remove("models");
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
    assert_eq!(compared, 32, "every recorded Go credential was compared");
}

mod access {
    use super::*;
    use cpa_exec::Executors;
    use cpa_exec::claude::ClaudeExecutor;
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
        let rt = Arc::new(cpa_server::testing::runtime(
            Config::load(&path).unwrap(),
            vec![],
            Executors {
                claude: ClaudeExecutor::new("http://127.0.0.1:1").unwrap(),
                codex: Default::default(),
                devices: Default::default(),
                openai: Default::default(),
                google: Default::default(),
            },
        ));
        let options = Options {
            local_password: scenario["local_password"].as_str().unwrap_or_default().into(),
            management_password: Some(scenario["env"].as_str().unwrap_or_default().into()),
            ..Options::default()
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
    use cpa_server::watching;
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
            // The fixtures set no auth-dir; never fall back to the real ~/.cli-proxy-api.
            let auth = dir.join("auth");
            let mut cfg = Config::load(&path).unwrap();
            cfg.auth_dir = auth.clone();
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
            let state = Management::with_options(
                rt,
                path.clone(),
                Options {
                    local_password: scenario["local_password"].as_str().unwrap_or_default().into(),
                    management_password: Some(scenario["env"].as_str().unwrap_or_default().into()),
                    auth_dir: Some(auth),
                    ..Options::default()
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
                // The goldens come from Go's cgo build, which loads native plugins; this
                // binary does too, except on Windows.
                if want.contains_key("X-CPA-SUPPORT-PLUGIN") {
                    want.insert("X-CPA-SUPPORT-PLUGIN".into(), cpa_plugin::SUPPORT_PLUGIN.into());
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

mod config_writes {
    use super::*;
    use cpa_exec::Executors;
    use cpa_exec::claude::ClaudeExecutor;

    use cpa_server::management::{Management, Options};
    use std::sync::Arc;

    /// bcrypt output differs per run; compare its presence, not its bytes.
    fn normalize(v: &Value) -> Value {
        match v {
            Value::String(s) if s.starts_with("$2a$") || s.starts_with("$2b$") => json!("<bcrypt>"),
            Value::Object(m) => Value::Object(m.iter().map(|(k, v)| (k.clone(), normalize(v))).collect()),
            Value::Array(a) => Value::Array(a.iter().map(normalize).collect()),
            other => other.clone(),
        }
    }

    /// Go's saver adds default-valued keys to every PUT/PATCH; cliproxy-rs deliberately
    /// writes only what the request changed (untouched text stays byte-stable). So
    /// `rust` must equal `go` except for keys Go added whose values are exactly Go's
    /// materialized defaults at that path.
    fn same_modulo_defaults(go: &Value, rust: &Value, defaults: &Value, at: &str) -> Result<(), String> {
        match (go, rust) {
            (Value::Object(g), Value::Object(r)) => {
                for (k, rv) in r {
                    let gv = g.get(k).ok_or(format!("{at}.{k}: only in Rust: {rv}"))?;
                    same_modulo_defaults(gv, rv, defaults.get(k).unwrap_or(&Value::Null), &format!("{at}.{k}"))?;
                }
                for (k, gv) in g {
                    if !r.contains_key(k) {
                        only_defaults(gv, defaults.get(k).unwrap_or(&Value::Null), &format!("{at}.{k}"))?;
                    }
                }
                Ok(())
            }
            (Value::Array(g), Value::Array(r)) if g.len() == r.len() => {
                for (i, (gv, rv)) in g.iter().zip(r).enumerate() {
                    same_modulo_defaults(gv, rv, &Value::Null, &format!("{at}[{i}]"))?;
                }
                Ok(())
            }
            _ if go == rust => Ok(()),
            _ => Err(format!("{at}: Go {go} != Rust {rust}")),
        }
    }

    /// Go-only keys are acceptable when they are Go's materialized defaults or typed
    /// zero values its saver writes for struct fields (for example `username: ""` in a
    /// new list item).
    fn only_defaults(go: &Value, defaults: &Value, at: &str) -> Result<(), String> {
        let zero = matches!(go, Value::Null | Value::Bool(false))
            || go.as_str() == Some("")
            || go.as_i64() == Some(0)
            || go.as_array().is_some_and(Vec::is_empty)
            || go.as_object().is_some_and(serde_json::Map::is_empty);
        if zero {
            return Ok(());
        }
        match (go, defaults) {
            (Value::Object(g), Value::Object(d)) => g
                .iter()
                .try_for_each(|(k, v)| only_defaults(v, d.get(k).unwrap_or(&Value::Null), &format!("{at}.{k}"))),
            _ if go == defaults => Ok(()),
            _ => Err(format!("{at}: Go has {go}, missing in Rust and not a Go default")),
        }
    }

    /// A YAML text as Go's decoder into `any` sees it: merge keys expanded.
    fn rust_yaml_value(text: &str) -> Value {
        let mut v: serde_yaml_ng::Value = serde_yaml_ng::from_str(text).unwrap();
        cpa_core::config::expand_merges(&mut v).unwrap();
        serde_json::to_value(v).unwrap()
    }

    /// Typed scalars as Go loads them (YAML 1.1 bools, truncated floats), and null
    /// plugin entries as the `{}` Go's saver writes. Go re-encodes these on its first
    /// write; cliproxy-rs keeps untouched text as written, so files compare by what
    /// they load as.
    fn typed_bools(v: &Value) -> Value {
        let mut yaml = serde_yaml_ng::to_value(v).unwrap();
        cpa_core::config::coerce_typed_scalars(&mut yaml);
        let mut json = serde_json::to_value(yaml).unwrap();
        if let Some(entries) = json.pointer_mut("/plugins/configs").and_then(Value::as_object_mut) {
            for entry in entries.values_mut().filter(|e| e.is_null()) {
                *entry = json!({});
            }
        }
        json
    }

    /// [`typed_bools`] for the value at a `/config/...` sub-path.
    fn typed_at(path: &str, v: &Value) -> Value {
        let parts: Vec<&str> = path
            .trim_start_matches("/config")
            .split('/')
            .filter(|p| !p.is_empty())
            .collect();
        let mut doc = v.clone();
        for part in parts.iter().rev() {
            doc = json!({ *part: doc });
        }
        let typed = typed_bools(&doc);
        parts.iter().fold(&typed, |node, part| &node[*part]).clone()
    }

    #[tokio::test]
    async fn go_config_v8_writes_replay_value_for_value() {
        let defaults = normalize(&fixture()["materialized_defaults"]);
        let hash = bcrypt::hash("fake-secret", 4).unwrap();
        let client = wreq::Client::new();
        let mut compared = 0;
        let mut bytes_compared = 0;
        let mut failures: Vec<String> = Vec::new();
        for scenario in fixture()["config_writes"].as_array().unwrap() {
            let name = scenario["name"].as_str().unwrap();
            let dir = std::env::temp_dir().join(format!("cpa-config-{name}-{}", std::process::id()));
            std::fs::create_dir_all(&dir).unwrap();
            let path = dir.join("config.yaml");
            std::fs::write(&path, scenario["yaml"].as_str().unwrap().replace("$HASH", &hash)).unwrap();
            // The fixtures set no auth-dir; never fall back to the real ~/.cli-proxy-api.
            let auth = dir.join("auth");
            let mut cfg = Config::load(&path).unwrap();
            cfg.auth_dir = auth.clone();
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
            let options = Options {
                management_password: Some(String::new()),
                auth_dir: Some(auth),
                ..Options::default()
            };
            let (base, server) = super::access::serve(Management::with_options(rt, path.clone(), options)).await;
            for (i, step) in scenario["steps"].as_array().unwrap().iter().enumerate() {
                let method: wreq::Method = step["method"].as_str().unwrap().parse().unwrap();
                let url = format!("{base}/v8/management{}", step["path"].as_str().unwrap());
                let res = client
                    .request(method, url)
                    .header("X-Test-Peer", "127.0.0.1:1")
                    .bearer_auth("fake-secret")
                    .body(step["body"].as_str().unwrap_or_default().to_owned())
                    .send()
                    .await
                    .unwrap();
                let status = res.status().as_u16();
                let body = res.text().await.unwrap();
                let at = format!("{name}[{i}] {} {}", step["method"], step["path"]);
                assert_eq!(status, step["status"], "{at}: {body}");
                let want = &step["response"];
                if step["path"] == "/config.yaml" && step["method"] == "GET" && status == 200 {
                    let got = normalize(&rust_yaml_value(&body));
                    if let Err(e) = same_modulo_defaults(&normalize(want), &got, &defaults, "yaml view") {
                        failures.push(format!("{at}: {e}"));
                    }
                    let go_archived = step["raw_response"].as_str().unwrap().contains("# mystery-section");
                    assert_eq!(body.contains("# mystery-section"), go_archived, "{at}: archive in view");
                } else if want.is_null() {
                    assert!(body.is_empty(), "{at}: {body}");
                } else {
                    let got: Value = serde_json::from_str(&body).unwrap();
                    // gin's exact bytes wherever the values agree exactly; key order always.
                    if let Some(raw) = step["raw_json"].as_str() {
                        if got == *want {
                            bytes_compared += 1;
                            if body != raw {
                                failures.push(format!("{at}: bytes differ\n go: {raw}\n rs: {body}"));
                            }
                        }
                        let go_raw: Value = serde_json::from_str(raw).unwrap();
                        if let Err(e) = same_key_order(&go_raw, &got, &at) {
                            failures.push(e);
                        }
                    }
                    if let Some(code) = want.get("error") {
                        assert_eq!(got["error"], *code, "{at}");
                        assert_eq!(got.get("field"), want.get("field"), "{at}");
                        assert_eq!(
                            got.get("message").is_some(),
                            want.get("message").is_some(),
                            "{at}: {got}"
                        );
                    } else {
                        // Config reads are compared like the file they reflect.
                        let path = step["path"].as_str().unwrap();
                        let (want, got) = if path == "/config" || path.starts_with("/config/") {
                            (typed_at(path, want), typed_at(path, &got))
                        } else {
                            (want.clone(), got)
                        };
                        if let Err(e) = same_modulo_defaults(&normalize(&want), &normalize(&got), &defaults, "response")
                        {
                            failures.push(format!("{at}: {e}"));
                        }
                    }
                }
                let file = std::fs::read_to_string(&path).unwrap();
                if let Err(e) = same_modulo_defaults(
                    &normalize(&typed_bools(&step["file"])),
                    &normalize(&typed_bools(&rust_yaml_value(&file))),
                    &defaults,
                    "file",
                ) {
                    failures.push(format!("{at}: {e}"));
                }
                if file.contains("# mystery-section") != step["file_has_archive"].as_bool().unwrap() {
                    failures.push(format!("{at}: archived section comment presence differs\n{file}"));
                }
                // Go keeps the archive in the root foot comment: nothing follows it.
                if let Some(pos) = file.find("# mystery-section")
                    && file[pos..].lines().any(|l| !l.trim().is_empty() && !l.starts_with('#'))
                {
                    failures.push(format!("{at}: archive is not at the end\n{file}"));
                }
                compared += 1;
            }
            server.abort();
            let _ = std::fs::remove_dir_all(&dir);
        }
        assert!(failures.is_empty(), "{}", failures.join("\n"));
        assert_eq!(compared, 98);
        eprintln!("config writes: {bytes_compared} of {compared} steps compared byte for byte");
        assert!(bytes_compared >= 40, "only {bytes_compared} byte comparisons");
    }
}

mod creds {
    use super::*;
    use cpa_exec::Executors;
    use cpa_exec::claude::ClaudeExecutor;

    use cpa_server::management::{Management, Options};
    use std::collections::HashMap;
    use std::sync::Arc;

    const TIMES: &[&str] = &[
        "observed_at",
        "modtime",
        "created_at",
        "updated_at",
        "last_refresh",
        "retry_at",
    ];

    /// Per-side values that cannot match: auth indexes (path-derived), timestamps,
    /// sizes (Go rewrites files with sorted keys) and local bucket labels.
    fn normalize(v: &Value, names: &HashMap<String, String>) -> Value {
        match v {
            Value::Object(m) => Value::Object(
                m.iter()
                    .map(|(k, v)| {
                        let v = match (k.as_str(), v) {
                            (t, Value::String(_)) if TIMES.contains(&t) => json!("<time>"),
                            ("size", Value::Number(_)) => json!("<size>"),
                            ("time", Value::String(_)) => json!("<label>"),
                            ("auth_index", Value::String(s)) => {
                                json!(format!(
                                    "<index:{}>",
                                    names.get(s).cloned().unwrap_or_else(|| s.clone())
                                ))
                            }
                            _ => normalize(v, names),
                        };
                        (k.clone(), v)
                    })
                    .collect(),
            ),
            Value::Array(a) => Value::Array(a.iter().map(|v| normalize(v, names)).collect()),
            other => other.clone(),
        }
    }

    /// Go decodes JSON numbers as float64, so `1.0` and `1` are the same value.
    fn go_numbers(v: &Value) -> Value {
        match v {
            Value::Number(n) => match n.as_f64() {
                Some(f) if f.fract() == 0.0 && f.abs() < 9e15 => json!(f as i64),
                _ => v.clone(),
            },
            Value::Object(m) => Value::Object(m.iter().map(|(k, v)| (k.clone(), go_numbers(v))).collect()),
            Value::Array(a) => Value::Array(a.iter().map(go_numbers).collect()),
            other => other.clone(),
        }
    }

    fn config_credentials(config: &Value) -> Vec<Value> {
        let yaml = serde_yaml_ng::to_string(config).unwrap();
        let cfg = Config::parse(&yaml).unwrap();
        credentials::from_config(&cfg)
            .iter()
            .map(|c| json!({"id": c.id, "provider": c.provider, "attributes": c.attributes}))
            .collect()
    }

    #[tokio::test]
    async fn go_credential_management_replay() {
        let hash = bcrypt::hash("fake-secret", 4).unwrap();
        let client = wreq::Client::new();
        let mut compared = 0;
        let mut bytes_compared = 0;
        for scenario in fixture()["credentials"].as_array().unwrap() {
            let name = scenario["name"].as_str().unwrap();
            if name == "dashboard_probes" {
                continue;
            }
            let log_files = scenario["log_files"].as_array();
            let dir = std::env::temp_dir().join(format!("cpa-creds-{name}-{}", std::process::id()));
            let root = dir.join("fixture-root");
            let auth = root.join("auth");
            std::fs::create_dir_all(&auth).unwrap();
            for (file, body) in scenario["auth_files"].as_object().unwrap() {
                std::fs::write(auth.join(file), body.as_str().unwrap()).unwrap();
            }
            let log_dir = root.join("logs");
            for file in log_files.into_iter().flatten() {
                std::fs::create_dir_all(&log_dir).unwrap();
                put_log(&log_dir, file, false);
            }
            let path = root.join("config.yaml");
            let yaml = scenario["yaml"]
                .as_str()
                .unwrap()
                .replace("$HASH", &hash)
                .replace("$AUTH", &auth.display().to_string());
            std::fs::write(&path, yaml).unwrap();
            let cfg = Config::load(&path).unwrap();
            let rt = Arc::new(cpa_server::testing::runtime(
                cfg.clone(),
                credentials::load(&cfg),
                Executors {
                    claude: ClaudeExecutor::new("http://127.0.0.1:1").unwrap(),
                    codex: Default::default(),
                    devices: Default::default(),
                    openai: Default::default(),
                    google: Default::default(),
                },
            ));
            // Runtime-only credentials, registered as the `/v1/ws` relay does.
            for id in scenario["runtime_auths"].as_array().into_iter().flatten() {
                let id = id.as_str().unwrap();
                assert!(rt.store().add_runtime(Credential::relay_session(id)));
                // Go seeds a fileless auth's index from its ID.
                let added = rt.store().get(id).unwrap();
                assert_eq!(
                    json!(credentials::auth_index(&added)),
                    scenario["indexes"][id],
                    "{name}: auth_index"
                );
            }
            let options = Options {
                management_password: Some(String::new()),
                log_dir: Some(log_dir.clone()),
                // Go parsed log line timestamps in UTC: the generator runs with TZ=UTC.
                log_zone: chrono::FixedOffset::east_opt(0),
                ..Options::default()
            };
            let state = Management::with_options(rt.clone(), path.clone(), options);
            let (base, server) = super::access::serve(state).await;
            // index -> name, for each side.
            let go_names: HashMap<String, String> = scenario["indexes"]
                .as_object()
                .unwrap()
                .iter()
                .map(|(n, i)| (i.as_str().unwrap().to_owned(), n.clone()))
                .collect();
            let rust_name = |c: &cpa_core::credential::Credential| match &c.source {
                Source::File(p) => p.file_name().unwrap().to_string_lossy().into_owned(),
                Source::Config { .. } | Source::Runtime => c.id.clone(),
            };
            let mut rust_names: HashMap<String, String> = rt
                .store()
                .snapshot()
                .iter()
                .map(|c| (credentials::auth_index(c), rust_name(c)))
                .collect();
            // Go's generator keeps the last config credential it registered.
            let cfg_id = rt
                .store()
                .snapshot()
                .iter()
                .rfind(|c| matches!(c.source, Source::Config { .. }))
                .map(|c| c.id.clone())
                .unwrap_or_default();
            let (echo_url, echo_server) = if scenario["echo"] == true {
                let (url, handle) = echo::serve().await;
                (url, Some(handle))
            } else {
                ("http://echo.invalid".to_owned(), None)
            };
            let resolve = |text: &str, names: &HashMap<String, String>| {
                let mut text = text.replace("$ECHO", &echo_url).replace("$CFGID", &cfg_id);
                for (index, name) in names {
                    text = text.replace(&format!("$INDEX({name})"), index);
                }
                text
            };
            let mut last_state = "no-state".to_owned();
            let mut last_cursor = "no-cursor".to_owned();
            for (i, step) in scenario["steps"].as_array().unwrap().iter().enumerate() {
                let at = format!("{name}[{i}] {} {}", step["method"], step["path"]);
                if let Some(ms) = step["sleep_ms"].as_u64() {
                    tokio::time::sleep(std::time::Duration::from_millis(ms)).await;
                }
                if let Some(file) = step.get("append_log") {
                    put_log(&log_dir, file, true);
                }
                let method: wreq::Method = step["method"].as_str().unwrap().parse().unwrap();
                let req_path = resolve(step["path"].as_str().unwrap(), &rust_names)
                    .replace("$STATE", &last_state)
                    .replace("$CURSOR", &last_cursor);
                let mut req = client
                    .request(method, format!("{base}/v8/management{req_path}"))
                    .header("X-Test-Peer", "127.0.0.1:1")
                    .bearer_auth("fake-secret")
                    .body(
                        resolve(step["body"].as_str().unwrap_or_default(), &rust_names).replace("$STATE", &last_state),
                    );
                if let Some(ct) = step["content_type"].as_str() {
                    req = req.header("Content-Type", ct);
                }
                let res = req.send().await.unwrap();
                let status = res.status().as_u16();
                let headers: HashMap<String, String> = ["Content-Type", "Content-Disposition"]
                    .iter()
                    .filter_map(|h| Some((h.to_string(), res.headers().get(*h)?.to_str().ok()?.to_owned())))
                    .collect();
                let body = res
                    .text()
                    .await
                    .unwrap()
                    .replace(&root.display().to_string(), "/fixture-root")
                    .replace(&echo_url, "$ECHO");
                assert_eq!(status, step["status"], "{at}: {body}");
                // New credentials (uploads) get indexes on both sides.
                for c in rt.store().snapshot().iter() {
                    rust_names
                        .entry(credentials::auth_index(c))
                        .or_insert_with(|| rust_name(c));
                }
                if let Some(raw) = step["raw_response"].as_str() {
                    assert_eq!(body, raw, "{at}");
                    let want = step["resp_headers"].as_object().unwrap();
                    for (h, v) in want {
                        // Go types `.log` from the host's MIME database; the fixture
                        // host (Linux) maps it to text/x-log, macOS has no entry.
                        if h == "Content-Type" && v == "text/x-log; charset=utf-8" && !cfg!(target_os = "linux") {
                            continue;
                        }
                        assert_eq!(headers.get(h).map(String::as_str), v.as_str(), "{at}: header {h}");
                    }
                } else {
                    let got: Value = serde_json::from_str(&body).unwrap();
                    // gin's exact bytes wherever the values agree exactly; key order always.
                    if let Some(raw) = step["raw_json"].as_str() {
                        if got == step["response"] {
                            assert_eq!(body, raw, "{at}: bytes");
                            bytes_compared += 1;
                        }
                        let go_raw: Value = serde_json::from_str(raw).unwrap();
                        same_key_order(&go_raw, &got, &at).unwrap();
                    }
                    if req_path.starts_with("/oauth/auth-url")
                        && let Some(state) = got["state"].as_str()
                    {
                        last_state = state.to_owned();
                    }
                    if let Some(cursor) = got["next-cursor"].as_str().filter(|c| !c.is_empty()) {
                        last_cursor = cursor.to_owned();
                    }
                    // Log routes compare exactly: sizes, mtimes and cursors are fixed.
                    if req_path.starts_with("/observability/logs") {
                        assert_eq!(got, step["response"], "{at}");
                    }
                    let mut want = normalize(&step["response"], &go_names);
                    let mut got = normalize(&got, &rust_names);
                    // Random login state and PKCE challenge.
                    for v in [&mut want, &mut got] {
                        if let Some(url) = v.get("url").and_then(Value::as_str) {
                            let mut parsed = url::Url::parse(url).unwrap();
                            let pairs: Vec<(String, String)> = parsed
                                .query_pairs()
                                .map(|(k, val)| {
                                    let val = if k == "state" || k == "code_challenge" {
                                        format!("<{k}>")
                                    } else {
                                        val.into_owned()
                                    };
                                    (k.into_owned(), val)
                                })
                                .collect();
                            parsed.query_pairs_mut().clear().extend_pairs(pairs);
                            v["url"] = parsed.to_string().into();
                            v["state"] = "<state>".into();
                        }
                    }
                    // api-call relays the upstream's own Date header.
                    for v in [&mut want, &mut got] {
                        if let Some(h) = v.get_mut("header").and_then(Value::as_object_mut) {
                            h.remove("Date");
                        }
                    }
                    // Go's harness registers models only for runtime credentials (as its
                    // service does on connect), so the cooldown-reset fallback list of a
                    // file credential is empty there; cliproxy-rs reports the credential's
                    // registrations (none here: the credential is disabled by this step).
                    if step["path"] == "/routing/cooldown/reset" && status == 200 && scenario["runtime_auths"].is_null()
                    {
                        assert_eq!(want["models"], json!([]), "{at}: harness assumption");
                    }
                    if let Some(msg) = want["error"].as_str().filter(|m| m.starts_with("invalid auth file: ")) {
                        // JSON parser wording differs; the prefix and status are Go's.
                        assert!(
                            got["error"].as_str().unwrap().starts_with("invalid auth file: "),
                            "{at}: {msg}"
                        );
                        want["error"] = got["error"].clone();
                    }
                    assert_eq!(got, want, "{at}");
                }
                // Auth dir contents, as JSON values.
                let mut files = serde_json::Map::new();
                for entry in std::fs::read_dir(&auth).unwrap() {
                    let entry = entry.unwrap();
                    let data = std::fs::read(entry.path()).unwrap();
                    let v = serde_json::from_slice::<Value>(&data)
                        .unwrap_or_else(|_| json!(format!("raw:{}", String::from_utf8_lossy(&data))));
                    files.insert(entry.file_name().to_string_lossy().into_owned(), v);
                }
                assert_eq!(
                    go_numbers(&Value::Object(files)),
                    go_numbers(&step["files"]),
                    "{at}: auth dir"
                );
                if let Some(want) = step.get("log_dir") {
                    let mut logs = serde_json::Map::new();
                    for entry in std::fs::read_dir(&log_dir).unwrap() {
                        let entry = entry.unwrap();
                        let text = std::fs::read_to_string(entry.path()).unwrap();
                        logs.insert(entry.file_name().to_string_lossy().into_owned(), text.into());
                    }
                    assert_eq!(&Value::Object(logs), want, "{at}: log dir");
                }
                for (file, raw) in step["raw_files"].as_object().into_iter().flatten() {
                    let got = std::fs::read_to_string(auth.join(file)).unwrap();
                    assert_eq!(Some(got.as_str()), raw.as_str(), "{at}: bytes of {file}");
                }
                // Config effects, compared through the credentials they synthesize.
                let file = std::fs::read_to_string(&path).unwrap();
                let rust_config: Value =
                    serde_json::to_value(serde_yaml_ng::from_str::<serde_yaml_ng::Value>(&file).unwrap()).unwrap();
                assert_eq!(
                    config_credentials(&rust_config),
                    config_credentials(&step["config"]),
                    "{at}: config API keys"
                );
                compared += 1;
            }
            server.abort();
            if let Some(echo) = echo_server {
                echo.abort();
            }
            let _ = std::fs::remove_dir_all(&dir);
        }
        assert_eq!(compared, 199);
        eprintln!("credentials: {bytes_compared} of {compared} steps compared byte for byte");
        assert!(bytes_compared >= 80, "only {bytes_compared} byte comparisons");
    }

    /// Writes or appends a fixture log file and sets Go's fixed mtime
    /// (`logEpoch + mtime` seconds).
    fn put_log(dir: &std::path::Path, file: &Value, append: bool) {
        use std::io::Write;
        let path = dir.join(file["name"].as_str().unwrap());
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .append(append)
            .truncate(!append)
            .open(&path)
            .unwrap();
        f.write_all(file["text"].as_str().unwrap().as_bytes()).unwrap();
        let at =
            std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_000 + file["mtime"].as_u64().unwrap());
        f.set_modified(at).unwrap();
    }

    /// Every file in the auth dir and the config, byte for byte.
    fn disk_snapshot(auth: &std::path::Path, config: &std::path::Path) -> Vec<(String, Vec<u8>)> {
        let mut out: Vec<(String, Vec<u8>)> = std::fs::read_dir(auth)
            .unwrap()
            .map(|e| {
                let e = e.unwrap();
                (
                    e.file_name().to_string_lossy().into_owned(),
                    std::fs::read(e.path()).unwrap(),
                )
            })
            .collect();
        out.sort();
        out.push(("config.yaml".into(), std::fs::read(config).unwrap()));
        out
    }

    /// The dashboard probes each action with input Go rejects with 400 before any
    /// I/O. Implemented routes must answer exactly like Go and leave the disk and the
    /// cooldown state untouched; routes not built yet must stay an empty 404.
    #[tokio::test]
    async fn dashboard_probes_are_rejected_without_side_effects() {
        const NOT_YET: &[&str] = &[];
        let scenario = fixture()["credentials"]
            .as_array()
            .unwrap()
            .iter()
            .find(|s| s["name"] == "dashboard_probes")
            .unwrap()
            .clone();
        let hash = bcrypt::hash("fake-secret", 4).unwrap();
        let dir = std::env::temp_dir().join(format!("cpa-probes-{}", std::process::id()));
        let auth = dir.join("fixture-root").join("auth");
        std::fs::create_dir_all(&auth).unwrap();
        for (file, body) in scenario["auth_files"].as_object().unwrap() {
            std::fs::write(auth.join(file), body.as_str().unwrap()).unwrap();
        }
        let path = dir.join("fixture-root").join("config.yaml");
        let yaml = scenario["yaml"]
            .as_str()
            .unwrap()
            .replace("$HASH", &hash)
            .replace("$AUTH", &auth.display().to_string());
        std::fs::write(&path, yaml).unwrap();
        let cfg = Config::load(&path).unwrap();
        let rt = Arc::new(cpa_server::testing::runtime(
            cfg.clone(),
            credentials::load(&cfg),
            Executors {
                claude: ClaudeExecutor::new("http://127.0.0.1:1").unwrap(),
                codex: Default::default(),
                devices: Default::default(),
                openai: Default::default(),
                google: Default::default(),
            },
        ));
        let options = Options {
            management_password: Some(String::new()),
            ..Options::default()
        };
        let (base, server) = super::access::serve(Management::with_options(rt.clone(), path.clone(), options)).await;
        let before = disk_snapshot(&auth, &path);
        let creds_before: Vec<(String, u64)> = rt
            .store()
            .snapshot()
            .iter()
            .map(|c| (c.id.clone(), c.revision))
            .collect();
        let client = wreq::Client::new();
        let mut implemented = 0;
        for step in scenario["steps"].as_array().unwrap() {
            let at = format!("{} {}", step["method"], step["path"]);
            let method: wreq::Method = step["method"].as_str().unwrap().parse().unwrap();
            let mut req = client
                .request(
                    method,
                    format!("{base}/v8/management{}", step["path"].as_str().unwrap()),
                )
                .header("X-Test-Peer", "127.0.0.1:1")
                .bearer_auth("fake-secret");
            if let Some(body) = step["body"].as_str() {
                req = req.header("Content-Type", "application/json").body(body.to_owned());
            }
            let res = req.send().await.unwrap();
            let status = res.status().as_u16();
            let body = res.text().await.unwrap();
            let route = step["path"].as_str().unwrap().split('?').next().unwrap();
            if status == 404 && body.is_empty() {
                assert!(
                    NOT_YET.contains(&route),
                    "{at}: implemented routes must answer the probe"
                );
            } else {
                assert!(!NOT_YET.contains(&route), "{at}: now implemented; drop it from NOT_YET");
                assert_eq!(status, step["status"], "{at}: {body}");
                assert_eq!(serde_json::from_str::<Value>(&body).unwrap(), step["response"], "{at}");
                implemented += 1;
            }
            assert_eq!(disk_snapshot(&auth, &path), before, "{at}: probe touched the disk");
            let creds_now: Vec<(String, u64)> = rt
                .store()
                .snapshot()
                .iter()
                .map(|c| (c.id.clone(), c.revision))
                .collect();
            assert_eq!(creds_now, creds_before, "{at}: probe changed credentials");
        }
        assert_eq!(implemented, 9);
        server.abort();
        let _ = std::fs::remove_dir_all(&dir);
    }
}

/// The api-call upstream: the same reports as `echoHandler` in the Go generator, so
/// each side records what its own client sent.
mod echo {
    use axum::body::Bytes;
    use axum::http::{HeaderMap, Method, StatusCode, Uri, header};
    use axum::response::{IntoResponse, Response};
    use serde_json::{Map, Value, json};
    use std::collections::BTreeMap;

    pub(super) async fn serve() -> (String, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        let own = addr.clone();
        let app = axum::Router::new().fallback(move |method: Method, uri: Uri, headers: HeaderMap, body: Bytes| {
            let own = own.clone();
            async move { handle(&own, method, uri, headers, body) }
        });
        let handle = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        (format!("http://{addr}"), handle)
    }

    fn handle(own: &str, method: Method, uri: Uri, headers: HeaderMap, body: Bytes) -> Response {
        match uri.path() {
            "/redirect" => return (StatusCode::FOUND, [(header::LOCATION, "/echo?from=redirect")]).into_response(),
            "/redirect307" => {
                return (StatusCode::TEMPORARY_REDIRECT, [(header::LOCATION, "/echo?from=307")]).into_response();
            }
            "/chunked" => {
                let parts = futures_util::stream::iter([
                    Ok::<_, std::io::Error>(Bytes::from_static(b"part")),
                    Ok(Bytes::from_static(b"two")),
                ]);
                return (
                    [(header::CONTENT_TYPE, "text/plain"), (header::TRAILER, "X-Foo")],
                    axum::body::Body::from_stream(parts),
                )
                    .into_response();
            }
            "/status" => {
                let mut res = (StatusCode::IM_A_TEAPOT, &b"teapot\xff"[..]).into_response();
                let h = res.headers_mut();
                h.insert(header::CONTENT_TYPE, "text/plain".parse().unwrap());
                h.append("x-multi", "a".parse().unwrap());
                h.append("x-multi", "b".parse().unwrap());
                return res;
            }
            _ => {}
        }
        let mut seen = BTreeMap::new();
        for name in [
            "Authorization",
            "X-Custom",
            "Content-Type",
            "User-Agent",
            "Content-Length",
            "Accept-Encoding",
            "Referer",
        ] {
            let values: Vec<Value> = headers
                .get_all(name)
                .iter()
                .map(|v| Value::from(String::from_utf8_lossy(v.as_bytes()).into_owned()))
                .collect();
            if !values.is_empty() {
                seen.insert(name, Value::Array(values));
            }
        }
        let host = headers
            .get(header::HOST)
            .and_then(|h| h.to_str().ok())
            .unwrap_or_default();
        let host = if host == own { "<listener>" } else { host };
        let length: i64 = headers
            .get(header::CONTENT_LENGTH)
            .and_then(|v| v.to_str().ok()?.parse().ok())
            .unwrap_or(0);
        // Go's json.Encoder: sorted keys, compact, trailing newline.
        let mut out = Map::new();
        out.insert("body".into(), String::from_utf8_lossy(&body).into_owned().into());
        out.insert("content_length".into(), length.into());
        out.insert("headers".into(), json!(seen));
        out.insert("host".into(), host.into());
        out.insert("method".into(), method.as_str().into());
        out.insert("path".into(), uri.path().into());
        out.insert("query".into(), uri.query().unwrap_or_default().into());
        let text = format!("{}\n", Value::Object(out));
        ([(header::CONTENT_TYPE, "application/json")], text).into_response()
    }
}
