//! Go v8.0.35 plugin-owned providers without a model router: a catalog-only plugin's
//! models bind to the credentials of their provider (with the credential's excluded
//! models applied), and a plugin executor serves those credentials through the
//! ordinary selection loop. Plugin schedulers choose among built-in credentials.
//! Private auth-dir, native test plugins, loopback-only upstreams.
#![cfg(unix)]

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use cpa_core::config::Config;
use cpa_exec::Executors;
use serde_json::{Value, json};

fn canned(records: &Path, label: &str, method: &str, result: Value) {
    let dir = records.join("respond").join(label);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join(format!("{method}.json")),
        json!({"ok":true,"result":result}).to_string(),
    )
    .unwrap();
}

fn calls(records: &Path, label: &str, method: &str) -> Vec<Value> {
    std::fs::read_to_string(records.join(format!("{label}.jsonl")))
        .unwrap_or_default()
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .filter(|r| r["method"] == method)
        .map(|r| serde_json::from_str(r["request"].as_str().unwrap()).unwrap())
        .collect()
}

/// A runtime with recorder plugins `(label, caps)` and auth files, serving on a local
/// port. `prepare` runs on the records dir before the plugins start.
struct Env {
    /// The plugin catalog is process-wide; tests in this binary take turns.
    _serial: tokio::sync::MutexGuard<'static, ()>,
    rt: Arc<cpa_server::Runtime>,
    dir: PathBuf,
    records: PathBuf,
    plugins: PathBuf,
    yaml: String,
    proxy: SocketAddr,
}

async fn env(
    name: &str,
    plugins: &[(&str, &str)],
    extra_yaml: &str,
    auths: &[(&str, &str)],
    prepare: impl FnOnce(&Path),
) -> Option<Env> {
    env_with(name, plugins, extra_yaml, auths, prepare, executors()).await
}

/// Executors whose built-in upstreams are dead local ports.
fn executors() -> Executors {
    Executors {
        claude: cpa_exec::claude::ClaudeExecutor::new("http://127.0.0.1:1").unwrap(),
        codex: Default::default(),
        devices: Default::default(),
        openai: Default::default(),
        google: Default::default(),
    }
}

/// [`env`] with these executors (for Codex live endpoints or Claude OAuth on a local
/// mock).
async fn env_with(
    name: &str,
    plugins: &[(&str, &str)],
    extra_yaml: &str,
    auths: &[(&str, &str)],
    prepare: impl FnOnce(&Path),
    executors: Executors,
) -> Option<Env> {
    static SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
    let serial = SERIAL.lock().await;
    let tmp = Path::new(env!("CARGO_TARGET_TMPDIR"));
    let built = cpa_plugin::testing::built_plugins(tmp, "plugin_owned")?;
    let dir = cpa_plugin::testing::scratch(tmp, name);
    let (auth_dir, records, plugin_dir) = (dir.join("auths"), dir.join("records"), dir.join("plugins"));
    for d in [&auth_dir, &records, &plugin_dir] {
        std::fs::create_dir_all(d).unwrap();
    }
    let mut yaml = format!(
        "auth-dir: {}\n{extra_yaml}plugins:\n  enabled: true\n  dir: {}\n  configs:\n",
        auth_dir.display(),
        plugin_dir.display()
    );
    for (label, caps) in plugins {
        std::fs::copy(built.join("recorder.so"), plugin_dir.join(format!("{label}.so"))).unwrap();
        yaml.push_str(&format!(
            "    {label}:\n      enabled: true\n      label: {label}\n      record: {}\n      caps: {caps}\n",
            records.display()
        ));
    }
    for (file, body) in auths {
        std::fs::write(auth_dir.join(file), body).unwrap();
    }
    prepare(&records);
    let cfg = Config::parse(&yaml).unwrap();
    let credentials = cpa_core::config::credentials::load(&cfg);
    let rt = Arc::new(cpa_server::testing::runtime(cfg, credentials, executors));
    cpa_server::plugins::start(&rt).await;
    let proxy = {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let app = cpa_server::router(rt.clone());
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        addr
    };
    Some(Env {
        _serial: serial,
        rt,
        dir,
        records,
        plugins: plugin_dir,
        yaml,
        proxy,
    })
}

impl Env {
    async fn chat(&self, model: &str) -> (u16, String) {
        self.post(format!(
            r#"{{"model":"{model}","messages":[{{"role":"user","content":"hi"}}]}}"#
        ))
        .await
    }

    async fn chat_stream(&self, model: &str) -> (u16, String) {
        self.post(format!(
            r#"{{"model":"{model}","stream":true,"messages":[{{"role":"user","content":"hi"}}]}}"#
        ))
        .await
    }

    async fn post(&self, body: String) -> (u16, String) {
        let response = wreq::Client::new()
            .post(format!("http://{}/v1/chat/completions", self.proxy))
            .header("content-type", "application/json")
            .body(body)
            .send()
            .await
            .unwrap();
        (response.status().as_u16(), response.text().await.unwrap())
    }

    /// Stops the refresh loop and waits for the refreshes it started. Stopping drops a
    /// refresh still waiting for its plugin, but one whose plugin answered commits (and
    /// writes its credential's file) after the loop is gone, holding the credential's
    /// prepare lock until it has.
    async fn stop_refreshes(&self) {
        self.rt.stop_auto_refresh();
        for credential in self.rt.store().snapshot() {
            drop(self.rt.store().prepare_lock(&credential.id).lock_owned().await);
        }
    }

    async fn finish(self) {
        self.stop_refreshes().await;
        self.rt.plugins().shutdown_all(None).await;
        std::fs::remove_dir_all(self.dir).unwrap();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn plugin_catalog_models_bind_to_credentials_served_by_a_plugin_executor() {
    // `catalog` lists models for provider `acme`; `acme` executes them.
    let Some(env) = env(
        "plugin-owned",
        &[("catalog", "model_provider"), ("acme", "executor")],
        "",
        &[(
            "acme.json",
            r#"{"type":"acme","token":"t-1","excluded_models":["acme-hidden"]}"#,
        )],
        |records| {
            canned(
                records,
                "catalog",
                "model.static",
                json!({"Provider":"ACME","Models":[{"ID":" acme-1 ","OwnedBy":"acme"},{"ID":"acme-hidden"}]}),
            )
        },
    )
    .await
    else {
        return;
    };
    let registry = env.rt.registry();
    assert!(registry.client_supports("acme.json", "acme-1"));
    assert!(
        !registry.client_supports("acme.json", "acme-hidden"),
        "excluded by the credential"
    );
    assert_eq!(
        registry.providers("acme-hidden"),
        Vec::<String>::new(),
        "no credential publishes it"
    );
    let client = wreq::Client::new();
    let models: Value = client
        .get(format!("http://{}/v1/models", env.proxy))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let ids: Vec<&str> = models["data"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|m| m["id"].as_str())
        .collect();
    assert_eq!(ids, ["acme-1"], "{models}");
    let (status, body) = env.chat("acme-1").await;
    assert_eq!(status, 200);
    // The recorder echoes the payload it was given.
    assert_eq!(
        body,
        r#"{"model":"acme-1","messages":[{"role":"user","content":"hi"}]}"#
    );
    let executed = calls(&env.records, "acme", "executor.execute");
    assert_eq!(executed.len(), 1);
    let request = &executed[0];
    assert_eq!(request["AuthID"], "acme.json", "relative to the auth dir");
    assert_eq!(request["AuthProvider"], "acme");
    assert_eq!(request["Model"], "acme-1");
    assert_eq!(request["AuthMetadata"]["token"], "t-1");
    let storage = base64::Engine::decode(
        &base64::engine::general_purpose::STANDARD,
        request["StorageJSON"].as_str().unwrap(),
    )
    .unwrap();
    let storage: Value = serde_json::from_slice(&storage).unwrap();
    assert_eq!(storage["token"], "t-1");
    // A model no credential carries is not routed to the executor.
    assert_ne!(env.chat("acme-hidden").await.0, 200);
    assert_eq!(calls(&env.records, "acme", "executor.execute").len(), 1);
    env.finish().await;
}

/// Go `registerModelsForAuth` appends plugin catalogs to OpenAI-compatible credentials
/// under their provider key, with no exclusions, whether or not models are configured.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn openai_compatible_credentials_list_their_provider_catalog() {
    let compat = "openai-compatibility:\n  - name: empty\n    base-url: http://127.0.0.1:1/v1\n    api-key-entries: [{api-key: k1}]\n  - name: full\n    base-url: http://127.0.0.1:1/v1\n    api-key-entries: [{api-key: k2}]\n    models: [{name: up-model}]\n";
    let Some(env) = env(
        "plugin-owned-compat",
        &[("cat-empty", "model_provider"), ("cat-full", "model_provider")],
        compat,
        &[],
        |records| {
            canned(
                records,
                "cat-empty",
                "model.static",
                json!({"Provider":"openai-compatible-empty","Models":[{"ID":"plug-a"}]}),
            );
            canned(
                records,
                "cat-full",
                "model.static",
                json!({"Provider":"openai-compatible-full","Models":[{"ID":"plug-b"}]}),
            );
        },
    )
    .await
    else {
        return;
    };
    let registry = env.rt.registry();
    let credential = |name: &str| {
        env.rt
            .store()
            .snapshot()
            .into_iter()
            .find(|c| c.attributes.get("compat_name").map(String::as_str) == Some(name))
            .unwrap()
            .id
            .clone()
    };
    let (empty, full) = (credential("empty"), credential("full"));
    assert!(registry.client_supports(&empty, "plug-a"), "no configured models");
    assert!(
        !registry.client_supports(&empty, "plug-b"),
        "another provider's catalog"
    );
    assert!(registry.client_supports(&full, "up-model"));
    assert!(registry.client_supports(&full, "plug-b"), "configured models");
    env.finish().await;
}

/// Go reserves a catalog's models only for a provider with a bound built-in executor
/// (`manager.Executor`); an OpenAI-compatible name no credential uses has none, so a
/// plugin executor offering the same model still claims it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_catalog_for_an_unbound_builtin_provider_reserves_nothing() {
    let Some(env) = env(
        "plugin-owned-unbound",
        &[("catalog", "model_provider"), ("acme", "model_provider,executor")],
        "",
        &[("acme.json", r#"{"type":"acme"}"#)],
        |records| {
            canned(
                records,
                "catalog",
                "model.static",
                json!({"Provider":"openai-compatible-unused","Models":[{"ID":"shared"}]}),
            );
            canned(
                records,
                "acme",
                "model.static",
                json!({"Provider":"acme","Models":[{"ID":"shared"}]}),
            );
        },
    )
    .await
    else {
        return;
    };
    assert!(env.rt.registry().client_supports("acme.json", "shared"));
    let (status, _) = env.chat("shared").await;
    assert_eq!(status, 200);
    assert_eq!(calls(&env.records, "acme", "executor.execute").len(), 1);
    env.finish().await;
}

/// A failed credential-bound plugin execution is an attempt on that credential. Go's
/// `rpcError` and plain errors are neither request- nor credential-scoped, so the auth
/// manager classifies them by status and body: each case below fails over to the other
/// credential and cools the failed one. A real transport fault cools nothing.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_failing_plugin_credential_fails_over_to_the_next() {
    let dead = "openai-compatibility:\n  - name: dead\n    base-url: http://127.0.0.1:1/v1\n    api-key-entries: [{api-key: k1}]\n    models: [{name: dead-model}]\n";
    let Some(env) = env(
        "plugin-owned-failover",
        &[("catalog", "model_provider"), ("acme", "executor")],
        dead,
        &[
            ("acme-a.json", r#"{"type":"acme"}"#),
            ("acme-b.json", r#"{"type":"acme"}"#),
        ],
        |records| {
            canned(
                records,
                "catalog",
                "model.static",
                json!({"Provider":"acme","Models":[{"ID":"acme-1"}]}),
            )
        },
    )
    .await
    else {
        return;
    };
    let not_found = r#"{"error":{"code":"model_not_found","message":"no such model"}}"#;
    // (status, message, cooldown of the failed credential in seconds, None for none).
    const HOUR: u64 = 3600;
    let cases = [
        (Some(429), "slow down", Some(1)),
        (Some(500), "boom", Some(60)),
        (Some(401), "expired", Some(HOUR / 2)),
        (Some(404), "not found", Some(12 * HOUR)),
        (Some(404), not_found, Some(12 * HOUR)),
        (Some(400), "requested model is not supported", Some(12 * HOUR)),
        // An application error without a status, like an unavailable executor.
        (None, "plugin broke", Some(60)),
        // Statusless lifecycle and transport text: Go fails over without a cooldown.
        (None, "broken pipe", None),
        (None, "unexpected EOF", None),
        (None, "context deadline exceeded", None),
        (None, "read tcp 10.0.0.1:443: connection reset by peer", None),
        // With a status the text is not consulted.
        (Some(500), "broken pipe", Some(60)),
        // A statusless model-not-found still skips the cooldown on lifecycle or transport
        // text, but the result's transient check ignores a TLS handshake timeout.
        (
            None,
            r#"{"error":{"code":"model_not_found","message":"broken pipe"}}"#,
            None,
        ),
        (
            None,
            r#"{"error":{"code":"model_not_found","message":"unexpected EOF"}}"#,
            None,
        ),
        (
            None,
            r#"{"error":{"code":"model_not_found","message":"tls handshake timeout"}}"#,
            Some(12 * HOUR),
        ),
    ];
    for (status, message, cool) in cases {
        let case = format!("{status:?} {message}");
        let dir = env.records.join("respond").join("acme");
        std::fs::create_dir_all(&dir).unwrap();
        let mut error = json!({"code":"x","message":message});
        if let Some(status) = status {
            error["http_status"] = json!(status);
        }
        std::fs::write(
            dir.join("executor.execute.once.json"),
            json!({"ok":false,"error":error}).to_string(),
        )
        .unwrap();
        let _ = std::fs::remove_file(env.records.join("acme.jsonl"));
        let (got, body) = env.chat("acme-1").await;
        assert_eq!(got, 200, "{case}: {body}");
        let ids: Vec<String> = calls(&env.records, "acme", "executor.execute")
            .into_iter()
            .map(|r| r["AuthID"].as_str().unwrap().to_owned())
            .collect();
        assert_eq!(ids.len(), 2, "{case}: {ids:?}");
        assert_ne!(ids[0], ids[1], "{case}: the second attempt uses the other credential");
        let cooling = env.rt.store().cooldowns(&ids[0]);
        match cool {
            None => assert!(cooling.is_empty(), "{case}: no cooldown: {cooling:?}"),
            Some(secs) => {
                assert_eq!(cooling.len(), 1, "{case}: the failed credential cools: {cooling:?}");
                assert_eq!(
                    (cooling[0].model.as_str(), cooling[0].status),
                    ("acme-1", status.unwrap_or(0)),
                    "{case}"
                );
                let left = cooling[0].remaining;
                assert!(
                    left <= Duration::from_secs(secs) && left > Duration::from_secs(secs.saturating_sub(5)),
                    "{case}: cools for {secs}s, {left:?} left"
                );
            }
        }
        assert!(env.rt.store().cooldowns(&ids[1]).is_empty(), "{case}");
        for id in ["acme-a.json", "acme-b.json"] {
            env.rt.store().reset_cooldown(id);
        }
    }
    // A request fault wins over transport text in the same statusless error: Go's
    // resultErrorFromError codes it request-scoped before the lifecycle and transient
    // checks, so the request stops on the first credential and nothing cools.
    for message in [
        r#"{"error":{"type":"invalid_request_error","message":"tls handshake timeout"}}"#,
        r#"{"error":{"type":"invalid_request_error","message":"broken pipe"}}"#,
    ] {
        let dir = env.records.join("respond").join("acme");
        std::fs::write(
            dir.join("executor.execute.once.json"),
            json!({"ok":false,"error":{"code":"x","message":message}}).to_string(),
        )
        .unwrap();
        let _ = std::fs::remove_file(env.records.join("acme.jsonl"));
        let (got, body) = env.chat("acme-1").await;
        assert_ne!(got, 200, "{message}: {body}");
        let ids: Vec<String> = calls(&env.records, "acme", "executor.execute")
            .into_iter()
            .map(|r| r["AuthID"].as_str().unwrap().to_owned())
            .collect();
        assert_eq!(ids.len(), 1, "{message}: no failover: {ids:?}");
        assert!(env.rt.store().cooldowns(&ids[0]).is_empty(), "{message}: no cooldown");
    }
    // A connection refused by a built-in executor's upstream is transport: no cooldown.
    let dead_id = env
        .rt
        .store()
        .snapshot()
        .into_iter()
        .find(|c| c.attributes.get("compat_name").map(String::as_str) == Some("dead"))
        .unwrap()
        .id
        .clone();
    assert_ne!(env.chat("dead-model").await.0, 200);
    assert!(
        env.rt.store().cooldowns(&dead_id).is_empty(),
        "transport faults do not cool"
    );
    env.finish().await;
}

/// Go 3b69068e: when translating the client body into the plugin executor's format
/// refuses it (an attachment the target cannot carry, leaving the turn empty), the route
/// answers 400 naming the part and the plugin is never called (`prepareExecutorCall`
/// returns the envelope's error).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_refused_translation_answers_400_without_calling_the_routed_executor() {
    // `rt` routes and executes in Claude format; `exo` executes in chat-completions.
    let Some(env) = env(
        "plugin-owned-refused-translation",
        &[
            ("rt", "model_router,executor\n      inputs: claude"),
            ("exo", "executor"),
        ],
        "",
        &[],
        |_| {},
    )
    .await
    else {
        return;
    };
    let route = |target_kind: &str, target: &str| {
        let dir = env.records.join("respond").join("rt");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("model.route.once.json"),
            json!({"ok":true,"result":{"Handled":true,"TargetKind":target_kind,"Target":target,"Reason":"test"}})
                .to_string(),
        )
        .unwrap();
    };
    let cases = [
        (
            "/v1/chat/completions",
            ("self", "rt"),
            r#"{"model":"m1","messages":[{"role":"user","content":[{"type":"file","file":{"file_id":"file-absent"}}]}]}"#,
            "file",
        ),
        (
            "/v1/responses",
            ("self", "rt"),
            r#"{"model":"m1","input":[{"role":"user","content":[{"type":"input_file","file_id":"file-absent"}]}]}"#,
            "input_file",
        ),
        (
            "/v1/messages",
            ("executor", "exo"),
            r#"{"model":"m1","max_tokens":5,"messages":[{"role":"user","content":[{"type":"container_upload","file_id":"file-absent"}]}]}"#,
            "container_upload",
        ),
    ];
    for (path, (kind, target), body, part) in cases {
        route(kind, target);
        for label in ["rt", "exo"] {
            let _ = std::fs::remove_file(env.records.join(format!("{label}.jsonl")));
        }
        let response = wreq::Client::new()
            .post(format!("http://{}{path}", env.proxy))
            .header("content-type", "application/json")
            .body(body)
            .send()
            .await
            .unwrap();
        let status = response.status().as_u16();
        let text = response.text().await.unwrap();
        assert_eq!(status, 400, "{path}: {text}");
        assert!(
            text.contains(&format!("unsupported content part: {part}")),
            "{path}: {text}"
        );
        assert_eq!(calls(&env.records, "rt", "model.route").len(), 1, "{path}: routed");
        assert!(
            calls(&env.records, target, "executor.execute").is_empty(),
            "{path}: not executed"
        );
    }
    env.finish().await;
}

/// A loopback OpenAI-compatible upstream that records each request's bearer key and
/// fails with 500 for the key in `fail`.
async fn keyed_upstream(seen: Arc<std::sync::Mutex<Vec<String>>>, fail: Arc<std::sync::Mutex<String>>) -> String {
    let handler = move |headers: axum::http::HeaderMap| {
        let (seen, fail) = (seen.clone(), fail.clone());
        async move {
            let key = headers["authorization"]
                .to_str()
                .unwrap()
                .trim_start_matches("Bearer ")
                .to_owned();
            seen.lock().unwrap().push(key.clone());
            if *fail.lock().unwrap() == key {
                return (axum::http::StatusCode::INTERNAL_SERVER_ERROR, "down".to_owned());
            }
            (
                axum::http::StatusCode::OK,
                r#"{"id":"r","object":"chat.completion","created":1,"model":"up-model","choices":[{"index":0,"message":{"role":"assistant","content":"ok"},"finish_reason":"stop"}]}"#.to_owned(),
            )
        }
    };
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move {
        axum::serve(listener, axum::Router::new().fallback(handler))
            .await
            .unwrap()
    });
    url
}

/// Go `pickViaPluginScheduler` in `pickNextMixedLegacy`: the plugin scheduler sees the
/// ready candidates of the highest priority tier (all tiers when it asks), without
/// secret attributes, and picks one, rejects, or delegates to a built-in strategy;
/// an unusable answer leaves the choice to the configured selector.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_plugin_scheduler_picks_rejects_or_delegates() {
    let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
    let fail = Arc::new(std::sync::Mutex::new(String::new()));
    let upstream = keyed_upstream(seen.clone(), fail.clone()).await;
    let yaml = format!(
        "openai-compatibility:\n  - name: up\n    base-url: {upstream}/v1\n    priority: 5\n    api-key-entries: [{{api-key: k-a}}, {{api-key: k-b}}]\n    models: [{{name: up-model}}]\n  - name: low\n    base-url: {upstream}/v1\n    api-key-entries: [{{api-key: k-low}}]\n    models: [{{name: up-model}}]\n"
    );
    // Codex live calls answer 201 with the `Thread-Id` as the call ID.
    let live_upstream = {
        let handler = |headers: axum::http::HeaderMap| async move {
            let id = headers["thread-id"].to_str().unwrap().to_owned();
            (
                axum::http::StatusCode::CREATED,
                [
                    ("location", format!("/v1/live/{id}")),
                    ("content-type", "application/sdp".to_owned()),
                ],
                "v=0",
            )
        };
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move {
            axum::serve(listener, axum::Router::new().fallback(handler))
                .await
                .unwrap()
        });
        url
    };
    for across in [false, true] {
        // The second run also turns session affinity on.
        let (caps, yaml) = if across {
            (
                "scheduler,scheduler_across_priorities",
                format!("{yaml}routing:\n  session-affinity: true\n"),
            )
        } else {
            ("scheduler", yaml.clone())
        };
        let codex = cpa_exec::codex::CodexExecutor::with_client(
            wreq::Client::new(),
            cpa_exec::codex_oauth::CodexOAuth::new(wreq::Client::new()),
        )
        .with_live_endpoints(
            format!("{live_upstream}/backend-api/codex/realtime/calls"),
            format!("ws{}/v1", live_upstream.trim_start_matches("http")),
        );
        let Some(env) = env_with(
            &format!("plugin-scheduler-{across}"),
            &[("sched", caps)],
            &yaml,
            &[(
                "codex.json",
                r#"{"type":"codex","access_token":"at-FAKE","account_id":"acct-FAKE"}"#,
            )],
            |_| {},
            Executors { codex, ..executors() },
        )
        .await
        else {
            return;
        };
        let key_of = |id: &str| env.rt.store().get(id).unwrap().attributes["api_key"].clone();
        let respond = |method: &str, once: bool, envelope: Value| {
            let dir = env.records.join("respond").join("sched");
            std::fs::create_dir_all(&dir).unwrap();
            let file = if once {
                format!("{method}.once.json")
            } else {
                format!("{method}.json")
            };
            std::fs::write(dir.join(file), envelope.to_string()).unwrap();
        };
        let picks = || calls(&env.records, "sched", "scheduler.pick");
        // Alpha Search selects one provider (Go `pickNextLegacy`), which the request
        // names, and returns a reject unenriched with Go's 503 fallback status.
        let alpha = || async {
            respond(
                "scheduler.pick",
                true,
                json!({"ok":true,"result":{"handled":true,"reject":true,"reject_code":"quota_hold","reject_reason":"wait"}}),
            );
            let _ = std::fs::remove_file(env.records.join("sched.jsonl"));
            let alpha = wreq::Client::new()
                .post(format!("http://{}/backend-api/codex/alpha/search", env.proxy))
                .header("x-custom", "v")
                .body(r#"{"id":"s-1","model":"gpt-5.5","query":"q"}"#)
                .send()
                .await
                .unwrap();
            assert_eq!(alpha.status().as_u16(), 503);
            assert_eq!(alpha.text().await.unwrap(), r#"{"error":"quota_hold: wait"}"#);
            let pick = picks().pop().expect("the scheduler was asked");
            assert_eq!(pick["Provider"], "codex");
            assert_eq!(pick["Providers"], json!(["codex"]));
            assert_eq!(pick["Options"]["Headers"]["X-Custom"], json!(["v"]));
            // The synthesized session header (Go `Header.Set` casing).
            assert_eq!(pick["Options"]["Headers"]["X-Session-Id"], json!(["s-1"]));
            assert_eq!(pick["Options"]["Metadata"]["session_affinity_provider"], "codex");
            assert!(
                pick["Options"]["Metadata"].get("canonical_session_id").is_none(),
                "no Enrich"
            );
            pick
        };
        let send_full = || async {
            let _ = std::fs::remove_file(env.records.join("sched.jsonl"));
            seen.lock().unwrap().clear();
            let response = wreq::Client::new()
                .post(format!("http://{}/v1/chat/completions", env.proxy))
                .header("content-type", "application/json")
                .header("x-custom", "v")
                .body(r#"{"model":"up-model","messages":[{"role":"user","content":"hi"}]}"#)
                .send()
                .await
                .unwrap();
            let headers = response.headers().clone();
            (response.status().as_u16(), headers, response.text().await.unwrap())
        };
        let send = || async {
            let (status, _, body) = send_full().await;
            (status, body)
        };

        // The recorder picks the last candidate.
        let (status, body) = send().await;
        assert_eq!(status, 200, "{body}");
        let pick = picks().pop().expect("the scheduler was asked");
        let candidates = pick["Candidates"].as_array().unwrap();
        let keys: Vec<String> = candidates.iter().map(|c| key_of(c["ID"].as_str().unwrap())).collect();
        let mut sorted = keys.clone();
        sorted.sort();
        let want: &[&str] = if across {
            &["k-a", "k-b", "k-low"]
        } else {
            &["k-a", "k-b"]
        };
        assert_eq!(sorted, want, "across={across}: the candidates");
        for c in candidates {
            let priority = if key_of(c["ID"].as_str().unwrap()) == "k-low" {
                0
            } else {
                5
            };
            assert_eq!(c["Priority"], priority);
            assert_eq!(c["Status"], "active");
            assert!(
                c["Attributes"].get("api_key").is_none(),
                "secret attributes stay out: {c}"
            );
            assert!(c["Attributes"].get("base_url").is_some(), "{c}");
        }
        let ids: Vec<&str> = candidates.iter().map(|c| c["ID"].as_str().unwrap()).collect();
        let mut by_id = ids.clone();
        by_id.sort();
        assert_eq!(ids, by_id, "candidates are ordered by ID");
        assert_eq!(pick["Provider"], "");
        assert_eq!(pick["Model"], "up-model");
        assert_eq!(pick["Stream"], false);
        assert_eq!(pick["Options"]["Headers"]["X-Custom"], json!(["v"]));
        let meta = &pick["Options"]["Metadata"];
        assert_eq!(meta["session_affinity_provider"], "mixed");
        assert_eq!(meta["session_affinity_model"], if across { "up-model" } else { "" });
        assert_eq!(meta["request_path"], "/v1/chat/completions");
        // Go `session.Enrich`: without an explicit session only the derived identity.
        assert!(
            meta["derived_session_id"].as_str().is_some_and(|d| !d.is_empty()),
            "{meta}"
        );
        assert!(meta.get("canonical_session_id").is_none(), "{meta}");
        assert_eq!(
            *seen.lock().unwrap(),
            [keys.last().unwrap().clone()],
            "the picked credential serves"
        );
        if across {
            // Go's affinity lookup cannot observe bindings a plugin scheduler made.
            let host = env.rt.plugins().clone();
            let instance = Arc::new(cpa_plugin::client::CallbackInstance::default());
            let guard = cpa_plugin::testing::callback_context(
                &host,
                "sched",
                instance.clone(),
                cpa_plugin::callbacks::RequestScope::default(),
            );
            let lookup = tokio::task::spawn_blocking(move || {
                cpa_plugin::testing::call_from_plugin(
                    &host,
                    "sched",
                    &instance,
                    "host.affinity.lookup",
                    br#"{"provider":"up","model":"up-model","session_id":"s1"}"#,
                )
                .unwrap()
            })
            .await
            .unwrap();
            drop(guard);
            let lookup: Value = serde_json::from_slice(&lookup).unwrap();
            assert_eq!(lookup["result"]["status"], "unsupported", "{lookup}");
            // Under session affinity Alpha's selector argument is the route model.
            assert_eq!(
                alpha().await["Options"]["Metadata"]["session_affinity_model"],
                "gpt-5.5"
            );
            env.finish().await;
            continue;
        }

        // An unknown ID leaves the choice to the configured selector.
        respond(
            "scheduler.pick",
            true,
            json!({"ok":true,"result":{"handled":true,"auth_id":"nope"}}),
        );
        assert_eq!(send().await.0, 200);
        assert_eq!(seen.lock().unwrap().len(), 1);

        // Rejects: Go's auth codes are enriched, another code keeps its text.
        respond(
            "scheduler.pick",
            true,
            json!({"ok":true,"result":{"handled":true,"reject":true}}),
        );
        let (status, headers, body) = send_full().await;
        assert_eq!(status, 503, "{body}");
        assert_eq!(
            body,
            r#"{"error":{"message":"auth_unavailable: scheduler rejected candidate selection (providers=openai-compatible-up,openai-compatible-low, model=up-model)","type":"server_error","code":"internal_server_error"}}"#
        );
        assert!(headers.get("retry-after").is_none(), "{headers:?}");
        for c in env.rt.store().snapshot() {
            assert!(
                env.rt.store().cooldowns(&c.id).is_empty(),
                "a reject cools nothing down"
            );
        }
        respond(
            "scheduler.pick",
            true,
            json!({"ok":true,"result":{"handled":true,"reject":true,"reject_code":"quota_hold","reject_reason":"wait"}}),
        );
        let (status, body) = send().await;
        assert_eq!(status, 500, "{body}");
        assert!(body.contains("quota_hold: wait"), "{body}");
        assert!(seen.lock().unwrap().is_empty(), "a reject sends nothing");

        assert_eq!(alpha().await["Options"]["Metadata"]["session_affinity_model"], "");

        // So does realtime, with Go's empty local model; any reject is a 503.
        respond(
            "scheduler.pick",
            true,
            json!({"ok":false,"error":{"code":"busy","message":"scheduler busy"}}),
        );
        let _ = std::fs::remove_file(env.records.join("sched.jsonl"));
        let live = wreq::Client::new()
            .post(format!("http://{}/v1/live", env.proxy))
            .header("content-type", "application/json")
            .header("x-custom", "v")
            .body(r#"{"sdp":"v=0"}"#)
            .send()
            .await
            .unwrap();
        assert_eq!(live.status().as_u16(), 503, "a statusless scheduler error");
        assert!(live.text().await.unwrap().contains("scheduler busy"));
        let pick = picks().pop().expect("the scheduler was asked");
        assert_eq!(pick["Provider"], "codex");
        assert_eq!(pick["Model"], "");
        assert_eq!(pick["Options"]["Headers"]["X-Custom"], json!(["v"]));
        assert_eq!(pick["Options"]["Metadata"]["session_affinity_provider"], "codex");
        assert_eq!(pick["Options"]["Metadata"]["session_affinity_model"], "");
        assert!(pick["Options"]["Metadata"].get("pinned_auth_id").is_none());

        // An existing call's selection (here a hangup) names its credential and call.
        let codex_id = env
            .rt
            .store()
            .snapshot()
            .into_iter()
            .find(|c| c.provider == "codex")
            .unwrap()
            .id
            .clone();
        respond(
            "scheduler.pick",
            true,
            json!({"ok":true,"result":{"handled":true,"auth_id":codex_id}}),
        );
        let created = wreq::Client::new()
            .post(format!("http://{}/v1/live", env.proxy))
            .header("content-type", "application/json")
            .header("thread-id", "call-1")
            .body(r#"{"sdp":"v=0"}"#)
            .send()
            .await
            .unwrap();
        assert_eq!(created.status().as_u16(), 201, "{}", created.text().await.unwrap());
        respond(
            "scheduler.pick",
            true,
            json!({"ok":true,"result":{"handled":true,"reject":true,"reject_code":"quota_hold","reject_reason":"wait"}}),
        );
        let _ = std::fs::remove_file(env.records.join("sched.jsonl"));
        let hangup = wreq::Client::new()
            .post(format!("http://{}/v1/realtime/calls/call-1/hangup", env.proxy))
            .send()
            .await
            .unwrap();
        assert_eq!(hangup.status().as_u16(), 503);
        let meta = picks().pop().expect("the scheduler was asked")["Options"]["Metadata"].clone();
        assert_eq!(meta["pinned_auth_id"], codex_id.as_str(), "{meta}");
        assert_eq!(meta["execution_session_id"], "call-1", "{meta}");
        assert_eq!(meta["session_affinity_model"], "", "{meta}");

        // A session Go's `ExtractSessionInfo` sees but `hasExplicitSession` does not
        // (`metadata.sessionID`) leaves only the derived identity, and a prompt cache key
        // is canonical without its conversation alias as a parent.
        for (body, canonical) in [
            (
                r#"{"model":"up-model","metadata":{"sessionID":"x"},"messages":[{"role":"user","content":"hi"}]}"#,
                None,
            ),
            (
                r#"{"model":"up-model","prompt_cache_key":"p1","conversation":{"id":"c1"},"messages":[{"role":"user","content":"hi"}]}"#,
                Some("pck:p1"),
            ),
        ] {
            let _ = std::fs::remove_file(env.records.join("sched.jsonl"));
            let response = wreq::Client::new()
                .post(format!("http://{}/v1/chat/completions", env.proxy))
                .header("content-type", "application/json")
                .body(body)
                .send()
                .await
                .unwrap();
            assert_eq!(response.status().as_u16(), 200);
            let meta = picks().pop().expect("the scheduler was asked")["Options"]["Metadata"].clone();
            match canonical {
                None => {
                    assert!(meta.get("canonical_session_id").is_none(), "{meta}");
                    assert!(
                        meta["derived_session_id"].as_str().is_some_and(|d| !d.is_empty()),
                        "{meta}"
                    );
                }
                Some(canonical) => {
                    assert_eq!(meta["canonical_session_id"], canonical, "{meta}");
                    assert!(meta.get("parent_session_id").is_none(), "{meta}");
                    assert!(meta.get("derived_session_id").is_none(), "{meta}");
                }
            }
        }

        // An explicit session is canonical, with its parent; no derived identity.
        let _ = std::fs::remove_file(env.records.join("sched.jsonl"));
        let explicit = wreq::Client::new()
            .post(format!("http://{}/v1/chat/completions", env.proxy))
            .header("content-type", "application/json")
            .header("x-session-id", "child")
            .body(r#"{"model":"up-model","parent_session_id":"parent","messages":[{"role":"user","content":"hi"}]}"#)
            .send()
            .await
            .unwrap();
        assert_eq!(explicit.status().as_u16(), 200);
        let meta = picks().pop().expect("the scheduler was asked")["Options"]["Metadata"].clone();
        assert_eq!(meta["canonical_session_id"], "header:child", "{meta}");
        assert_eq!(meta["parent_session_id"], "header:parent", "{meta}");
        assert!(meta.get("derived_session_id").is_none(), "{meta}");
        seen.lock().unwrap().clear();

        // The scheduler's own failure answers with its status.
        respond(
            "scheduler.pick",
            true,
            json!({"ok":false,"error":{"code":"busy","message":"scheduler busy","http_status":429}}),
        );
        let (status, body) = send().await;
        assert_eq!(status, 429, "{body}");

        // Delegation to the built-in strategies, within the top tier.
        respond(
            "scheduler.pick",
            false,
            json!({"ok":true,"result":{"handled":true,"delegate_builtin":"fill-first"}}),
        );
        let mut served = Vec::new();
        for _ in 0..3 {
            assert_eq!(send().await.0, 200);
            served.extend(seen.lock().unwrap().clone());
        }
        // Fill-first takes the top tier's first credential by ID.
        let first_by_id = candidates
            .iter()
            .map(|c| c["ID"].as_str().unwrap())
            .min()
            .map(key_of)
            .unwrap();
        assert_eq!(served, [first_by_id.as_str(); 3], "fill-first stays on one credential");
        respond(
            "scheduler.pick",
            false,
            json!({"ok":true,"result":{"handled":true,"delegate_builtin":"round-robin"}}),
        );
        let mut served = Vec::new();
        for _ in 0..4 {
            assert_eq!(send().await.0, 200);
            served.extend(seen.lock().unwrap().clone());
        }
        served.sort();
        assert_eq!(served, ["k-a", "k-a", "k-b", "k-b"], "round-robin alternates");

        // A failed attempt is not offered again: the retry's candidates exclude it.
        std::fs::remove_file(env.records.join("respond").join("sched").join("scheduler.pick.json")).unwrap();
        let first = keys.last().unwrap().clone();
        *fail.lock().unwrap() = first.clone();
        let (status, body) = send().await;
        assert_eq!(status, 200, "{body}");
        let picks = picks();
        assert_eq!(picks.len(), 2, "one pick per attempt");
        let retry: Vec<String> = picks[1]["Candidates"]
            .as_array()
            .unwrap()
            .iter()
            .map(|c| key_of(c["ID"].as_str().unwrap()))
            .collect();
        assert!(!retry.contains(&first), "{retry:?}");
        fail.lock().unwrap().clear();
        env.finish().await;
    }
}

/// Go's plugin availability pass does not filter by weight: under weighted round-robin
/// a ready weight-0 credential is offered and can be picked, and with every weight at
/// zero the scheduler is still asked. A picked credential that stopped being ready
/// while the plugin answered falls back to the configured selector (Go serves it from
/// its snapshot; docs/DIFFERENCES-FROM-GO.md).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_plugin_scheduler_sees_zero_weights_and_a_stale_pick_falls_back() {
    let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
    let fail = Arc::new(std::sync::Mutex::new(String::new()));
    let upstream = keyed_upstream(seen.clone(), fail).await;
    let yaml = format!(
        "routing:\n  strategy: weighted-round-robin\nopenai-compatibility:\n  - name: up\n    base-url: {upstream}/v1\n    api-key-entries: [{{api-key: k-a, weight: 0}}, {{api-key: k-b, weight: 1}}]\n    models: [{{name: up-model}}]\n"
    );
    let Some(env) = env(
        "plugin-scheduler-weights",
        &[("sched", "scheduler")],
        &yaml,
        &[],
        |_| {},
    )
    .await
    else {
        return;
    };
    let credential = |key: &str| {
        env.rt
            .store()
            .snapshot()
            .into_iter()
            .find(|c| c.attributes.get("api_key").map(String::as_str) == Some(key))
            .unwrap()
    };
    let dir = env.records.join("respond").join("sched");
    std::fs::create_dir_all(&dir).unwrap();
    let pick_once = |key: &str| {
        let envelope = json!({"ok":true,"result":{"handled":true,"auth_id":credential(key).id}});
        std::fs::write(dir.join("scheduler.pick.once.json"), envelope.to_string()).unwrap();
    };
    let picks = || calls(&env.records, "sched", "scheduler.pick");
    let send = || async {
        seen.lock().unwrap().clear();
        let response = wreq::Client::new()
            .post(format!("http://{}/v1/chat/completions", env.proxy))
            .header("content-type", "application/json")
            .body(r#"{"model":"up-model","messages":[{"role":"user","content":"hi"}]}"#)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status().as_u16(), 200, "{}", response.text().await.unwrap());
        seen.lock().unwrap().clone()
    };

    // The weight-0 credential is offered and serves when picked.
    pick_once("k-a");
    assert_eq!(send().await, ["k-a"]);
    let offered: Vec<Value> = picks().pop().unwrap()["Candidates"].as_array().unwrap().clone();
    assert_eq!(offered.len(), 2, "{offered:?}");

    // k-a stops being ready while the plugin answers: weighted selection serves k-b.
    pick_once("k-a");
    let hold = dir.join("scheduler.pick.hold");
    std::fs::write(&hold, "").unwrap();
    let before = picks().len();
    let disable = async {
        while picks().len() == before {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        let mut off = (*credential("k-a")).clone();
        off.disabled = true;
        env.rt.store().upsert(off);
        std::fs::remove_file(&hold).unwrap();
    };
    let (served, ()) = tokio::join!(send(), disable);
    assert_eq!(served, ["k-b"]);

    // Every weight at zero: the scheduler is still asked, and its pick serves.
    let mut zero = (*credential("k-b")).clone();
    zero.attributes.insert("weight".into(), "0".into());
    env.rt.store().upsert(zero);
    pick_once("k-b");
    let before = picks().len();
    assert_eq!(send().await, ["k-b"]);
    assert_eq!(picks().len(), before + 1);
    env.finish().await;
}

/// Go keeps the cooldown skip and the retry round apart: a statusless lifecycle text
/// (`context deadline exceeded`) skips the cooldown but earns no request-retry round,
/// while transient transport text (`broken pipe`) earns one (`isRequestRetryRoundError`).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn statusless_plugin_errors_earn_retry_rounds_only_for_transport_text() {
    for retry in [0, 1] {
        let Some(env) = env(
            &format!("plugin-owned-retry-{retry}"),
            &[("catalog", "model_provider"), ("acme", "executor")],
            &format!("request-retry: {retry}\n"),
            &[
                ("acme-a.json", r#"{"type":"acme"}"#),
                ("acme-b.json", r#"{"type":"acme"}"#),
            ],
            |records| {
                canned(
                    records,
                    "catalog",
                    "model.static",
                    json!({"Provider":"acme","Models":[{"ID":"acme-1"}]}),
                )
            },
        )
        .await
        else {
            return;
        };
        for (message, rounds) in [("context deadline exceeded", 1), ("broken pipe", 1 + retry)] {
            let case = format!("request-retry {retry}, {message}");
            let dir = env.records.join("respond").join("acme");
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(
                dir.join("executor.execute.json"),
                json!({"ok":false,"error":{"code":"x","message":message}}).to_string(),
            )
            .unwrap();
            let _ = std::fs::remove_file(env.records.join("acme.jsonl"));
            let (got, body) = env.chat("acme-1").await;
            assert_ne!(got, 200, "{case}: {body}");
            let attempts = calls(&env.records, "acme", "executor.execute").len();
            assert_eq!(attempts, 2 * rounds, "{case}: both credentials once per round");
            for id in ["acme-a.json", "acme-b.json"] {
                assert!(env.rt.store().cooldowns(id).is_empty(), "{case}: {id} does not cool");
            }
        }
        env.finish().await;
    }
}

/// Go hands a plugin stream's chunk error to the auth manager's stream bootstrap, which
/// classifies it like an execution error: an error before the first payload fails over
/// (conductor_stream.go:345-460); one after payload has been sent ends the stream.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_plugin_stream_failing_before_its_first_chunk_fails_over() {
    let Some(env) = env(
        "plugin-owned-stream-failover",
        &[("catalog", "model_provider"), ("acme", "executor")],
        "",
        &[
            ("acme-a.json", r#"{"type":"acme"}"#),
            ("acme-b.json", r#"{"type":"acme"}"#),
        ],
        |records| {
            canned(
                records,
                "catalog",
                "model.static",
                json!({"Provider":"acme","Models":[{"ID":"acme-1"}]}),
            )
        },
    )
    .await
    else {
        return;
    };
    let script = |chunks: Value, error: &str| {
        let dir = env.records.join("respond").join("acme");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("executor.execute_stream.script.once.json"),
            json!({"chunks":chunks,"error":error}).to_string(),
        )
        .unwrap();
        let _ = std::fs::remove_file(env.records.join("acme.jsonl"));
    };
    let attempts = || -> Vec<String> {
        calls(&env.records, "acme", "executor.execute_stream")
            .into_iter()
            .map(|r| r["AuthID"].as_str().unwrap().to_owned())
            .collect()
    };
    // (first-chunk error, cooldown of the failed credential in seconds).
    for (error, cool) in [("plugin stream broke", Some(60)), ("broken pipe", None)] {
        script(json!([]), error);
        let (got, body) = env.chat_stream("acme-1").await;
        assert_eq!(got, 200, "{error}: {body}");
        assert!(body.contains("one") && body.contains("three"), "{error}: {body}");
        let ids = attempts();
        assert_eq!(ids.len(), 2, "{error}: {ids:?}");
        assert_ne!(ids[0], ids[1], "{error}: the second attempt uses the other credential");
        let cooling = env.rt.store().cooldowns(&ids[0]);
        match cool {
            None => assert!(cooling.is_empty(), "{error}: {cooling:?}"),
            Some(secs) => {
                assert_eq!(cooling.len(), 1, "{error}: {cooling:?}");
                assert_eq!((cooling[0].model.as_str(), cooling[0].status), ("acme-1", 0), "{error}");
                let left = cooling[0].remaining;
                assert!(
                    left <= Duration::from_secs(secs) && left > Duration::from_secs(secs - 5),
                    "{error}: {left:?}"
                );
            }
        }
        for id in ["acme-a.json", "acme-b.json"] {
            env.rt.store().reset_cooldown(id);
        }
    }
    // After payload has reached the client the stream ends; there is no second attempt.
    script(json!(["data: early\n\n"]), "plugin stream broke late");
    let (got, body) = env.chat_stream("acme-1").await;
    assert_eq!(got, 200, "{body}");
    assert!(body.contains("early") && !body.contains("three"), "{body}");
    assert_eq!(attempts().len(), 1);
    env.finish().await;
}

/// A plugin executor may claim a built-in provider while no credential of it exists, but
/// Go rebinds the built-in executor as soon as such a credential arrives
/// (`registerExecutorForAuth`), before any plugin sync. Credentials reach the store
/// without a sync both from a management publish (the store reconciles after the sync is
/// signalled) and from a plugin login (`upsert`).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_new_builtin_credential_never_reaches_a_plugin_executor_for_its_provider() {
    let Some(env) = env(
        "plugin-owned-builtin-rebind",
        &[("catalog", "model_provider"), ("claude", "executor")],
        "",
        &[],
        |records| {
            canned(
                records,
                "catalog",
                "model.static",
                json!({"Provider":"claude","Models":[{"ID":"claude-plug-1"}]}),
            )
        },
    )
    .await
    else {
        return;
    };
    let cfg = env.rt.config();
    let credential = |file: &str| {
        let path = cfg.auth_dir.join(file);
        let body = br#"{"type":"claude","access_token":"fake"}"#;
        std::fs::write(&path, body).unwrap();
        cpa_core::config::credentials::from_file(&cfg, &cfg.auth_dir, &path, body)
            .unwrap()
            .unwrap()
    };
    let check = |stage: &str, (got, body): (u16, String)| {
        assert_ne!(got, 200, "{stage}: the built-in executor's dead upstream: {body}");
        assert!(
            calls(&env.records, "claude", "executor.execute").is_empty(),
            "{stage}: the plugin executor served a built-in credential"
        );
    };
    // A management publish: the store reconciles with the new file, no sync follows.
    let mut all: Vec<_> = env.rt.store().snapshot().iter().map(|c| (**c).clone()).collect();
    all.push(credential("claude-a.json"));
    env.rt.store().reconcile(all);
    check("publish", env.chat("claude-plug-1").await);
    // A plugin login: the credential is upserted, no sync follows.
    let rest: Vec<_> = env
        .rt
        .store()
        .snapshot()
        .iter()
        .filter(|c| c.provider != "claude")
        .map(|c| (**c).clone())
        .collect();
    env.rt.store().reconcile(rest);
    env.rt.store().upsert(credential("claude-b.json"));
    check("plugin login", env.chat("claude-plug-1").await);
    env.finish().await;
}

/// The executor table keeps the plugin instance it registered (Go `executorAdapter`
/// with path and version). A hot replacement that identifies another provider must
/// not receive the old provider's credentials before its own registration commits.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_replaced_executor_plugin_does_not_serve_its_predecessors_credentials() {
    let Some(env) = env(
        "plugin-owned-replace",
        &[("catalog", "model_provider"), ("acme", "executor")],
        "",
        &[("acme.json", r#"{"type":"acme","token":"t-1"}"#)],
        |records| {
            canned(
                records,
                "catalog",
                "model.static",
                json!({"Provider":"acme","Models":[{"ID":"acme-1"}]}),
            )
        },
    )
    .await
    else {
        return;
    };
    assert_eq!(env.chat("acme-1").await.0, 200);
    // v2 identifies as `acme2`. Apply it on the host alone: the plugin sync that would
    // re-register executors has not run yet.
    std::fs::copy(env.plugins.join("acme.so"), env.plugins.join("acme-v2.so")).unwrap();
    let yaml = env.yaml.replace("label: acme\n", "label: acme2\n");
    env.rt
        .plugins()
        .apply_config(Arc::new(Config::parse(&yaml).unwrap()))
        .await;
    let active = env.rt.plugins().active_records();
    assert!(active.iter().any(|r| r.id == "acme" && r.version == "2"), "v2 is live");
    let (status, _) = env.chat("acme-1").await;
    assert_ne!(status, 200);
    assert!(
        calls(&env.records, "acme2", "executor.execute").is_empty(),
        "v2 received a credential registered for v1"
    );
    env.finish().await;
}

/// Removing a built-in provider's catalog frees its models for a plugin executor in
/// the same sync, although ownership was first decided against the old catalog.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn removing_a_builtin_catalog_hands_its_model_to_a_plugin_executor() {
    let codex = "api-keys:\n  codex:\n    - base-url: http://127.0.0.1:1\n      models: [{name: gpt-fixture}]\n      keys: [{api-key: sk-fake}]\n";
    let Some(env) = env(
        "plugin-owned-handover",
        &[("catalog", "model_provider"), ("acme", "model_provider,executor")],
        codex,
        &[("acme.json", r#"{"type":"acme"}"#)],
        |records| {
            canned(
                records,
                "catalog",
                "model.static",
                json!({"Provider":"codex","Models":[{"ID":"shared"}]}),
            );
            canned(
                records,
                "acme",
                "model.static",
                json!({"Provider":"acme","Models":[{"ID":"shared"}]}),
            );
        },
    )
    .await
    else {
        return;
    };
    assert!(
        !env.rt.registry().client_supports("acme.json", "shared"),
        "reserved for codex"
    );
    let yaml = env.yaml.replace(
        "    catalog:\n      enabled: true\n",
        "    catalog:\n      enabled: false\n",
    );
    assert_ne!(yaml, env.yaml);
    cpa_server::plugins::sync(&env.rt, Arc::new(Config::parse(&yaml).unwrap())).await;
    assert!(env.rt.registry().client_supports("acme.json", "shared"));
    let (status, body) = env.chat("shared").await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(calls(&env.records, "acme", "executor.execute").len(), 1);
    env.finish().await;
}

/// Go binds a built-in executor to an unknown provider (the OpenAI-compatible one)
/// only when no plugin executor is a candidate for it or a native compatibility config
/// names it. An auth file typed `openai-compatible-acme` without config goes to the
/// plugin; a configured entry, with keys or without, keeps the built-in executor.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_plugin_executor_takes_an_unconfigured_compatible_provider() {
    let catalog = |records: &Path| {
        canned(
            records,
            "acme",
            "model.static",
            json!({"Provider":"openai-compatible-acme","Models":[{"ID":"acme-m"}]}),
        )
    };
    let plugins = [("acme", "model_provider,executor")];
    let Some(unconfigured) = env(
        "plugin-owned-compat-candidate",
        &plugins,
        "",
        &[("acme.json", r#"{"type":"openai-compatible-acme"}"#)],
        catalog,
    )
    .await
    else {
        return;
    };
    let (status, body) = unconfigured.chat("acme-m").await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(calls(&unconfigured.records, "acme", "executor.execute").len(), 1);
    unconfigured.finish().await;
    for keys in ["    api-key-entries: [{api-key: k1}]\n", ""] {
        let config = format!("openai-compatibility:\n  - name: acme\n    base-url: http://127.0.0.1:1/v1\n{keys}");
        let Some(configured) = env("plugin-owned-compat-native", &plugins, &config, &[], catalog).await else {
            return;
        };
        let env = configured;
        let case = if keys.is_empty() { "keyless" } else { "keyed" };
        let credential = env
            .rt
            .store()
            .snapshot()
            .into_iter()
            .find(|c| c.attributes.get("compat_name").map(String::as_str) == Some("acme"))
            .unwrap_or_else(|| panic!("{case}: a configured credential"));
        assert!(
            env.rt.registry().client_supports(&credential.id, "acme-m"),
            "{case}: the catalog binds to the configured credential"
        );
        assert_ne!(env.chat("acme-m").await.0, 200, "{case}: the dead native upstream");
        assert!(
            calls(&env.records, "acme", "executor.execute").is_empty(),
            "{case}: the built-in executor keeps the provider"
        );
        env.finish().await;
    }
}

/// The next `count` usage records, as JSON; a stream's record comes as it ends.
async fn usage_records(rt: &cpa_server::Runtime, count: usize) -> Vec<Value> {
    let mut records = Vec::new();
    for _ in 0..200 {
        records.extend(rt.usage_queue().pop_oldest(count - records.len()));
        if records.len() == count {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(records.len(), count);
    records.iter().map(|r| serde_json::from_slice(r).unwrap()).collect()
}

const USAGE_ON: &str = "observability:\n  usage:\n    usage-statistics-enabled: true\n";

/// Go `TestExecutorAdapterExecuteAttributesResponsesUsageToSelectedAuth`: a
/// credential-bound plugin executor's buffered answer publishes its usage, read from
/// the plugin's Responses output (an event's `response.usage`, else the completed
/// object's top-level usage), with the reasoning effort of the payload it was sent.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_plugin_executor_publishes_the_usage_of_its_own_answer() {
    let Some(env) = env(
        "plugin-owned-usage",
        &[
            ("catalog", "model_provider"),
            ("acme", "executor\n      inputs: responses\n      outputs: responses"),
        ],
        USAGE_ON,
        &[("acme.json", r#"{"type":"acme"}"#)],
        |records| {
            canned(
                records,
                "catalog",
                "model.static",
                json!({"Provider":"acme","Models":[{"ID":"acme-1"}]}),
            )
        },
    )
    .await
    else {
        return;
    };
    env.rt.usage_queue().configure(true, &env.rt.config());
    let answers = [
        // A completed Responses object: top-level usage.
        (
            r#"{"id":"resp_1","object":"response","service_tier":"default","usage":{"input_tokens":34,"output_tokens":499,"total_tokens":533}}"#,
            (34, 499, 533),
        ),
        // A terminal event: `response.usage`, which the client-format parser misses.
        (
            r#"{"type":"response.completed","response":{"id":"resp_2","usage":{"input_tokens":5,"output_tokens":7,"total_tokens":12}}}"#,
            (5, 7, 12),
        ),
    ];
    for (answer, expected) in answers {
        let payload = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, answer);
        canned(&env.records, "acme", "executor.execute", json!({"Payload": payload}));
        let response = wreq::Client::new()
            .post(format!("http://{}/v1/responses", env.proxy))
            .header("content-type", "application/json")
            .body(r#"{"model":"acme-1","input":"Write me a poem","reasoning":{"effort":"high"}}"#)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status().as_u16(), 200, "{answer}");
        let record = &usage_records(&env.rt, 1).await[0];
        assert_eq!(record["provider"], "acme", "{record}");
        assert_eq!(record["executor_type"], "executorAdapter", "{record}");
        assert_eq!(record["stream"], false, "{record}");
        assert_eq!(record["failed"], false, "{record}");
        assert_eq!(record["reasoning_effort"], "high", "{record}");
        let tokens = &record["tokens"];
        let got = (
            tokens["input_tokens"].as_i64().unwrap(),
            tokens["output_tokens"].as_i64().unwrap(),
            tokens["total_tokens"].as_i64().unwrap(),
        );
        assert_eq!(got, expected, "{record}");
    }
    env.finish().await;
}

/// Go `TestExecutorAdapterExecuteStreamSplitAcrossBraceBoundary`,
/// `…TwoCompleteChunksWithoutNewline` and `…ErrorChunk`: a plugin stream's usage is
/// read from its own lines, however its chunks split them, and a stream that fails
/// after its usage line publishes the failure with that usage
/// (`StreamUsageBuffer.PublishFailure`).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_plugin_executor_stream_publishes_its_usage_even_when_it_fails() {
    let Some(env) = env(
        "plugin-owned-stream-usage",
        &[("catalog", "model_provider"), ("acme", "executor")],
        USAGE_ON,
        &[("acme.json", r#"{"type":"acme"}"#)],
        |records| {
            canned(
                records,
                "catalog",
                "model.static",
                json!({"Provider":"acme","Models":[{"ID":"acme-1"}]}),
            )
        },
    )
    .await
    else {
        return;
    };
    env.rt.usage_queue().configure(true, &env.rt.config());
    let script = |chunks: Value, error: &str| {
        let dir = env.records.join("respond").join("acme");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("executor.execute_stream.script.once.json"),
            json!({"chunks":chunks,"error":error}).to_string(),
        )
        .unwrap();
    };
    let tokens = |record: &Value| {
        let t = &record["tokens"];
        (
            t["input_tokens"].as_i64().unwrap(),
            t["output_tokens"].as_i64().unwrap(),
            t["total_tokens"].as_i64().unwrap(),
        )
    };
    // Split just before the usage object's opening brace.
    script(
        json!([
            "data: {\"choices\":[],\"usage\":",
            "{\"prompt_tokens\":18,\"completion_tokens\":22,\"total_tokens\":40}}\n\n",
            "data: [DONE]\n\n"
        ]),
        "",
    );
    let (got, body) = env.chat_stream("acme-1").await;
    assert_eq!(got, 200, "{body}");
    let record = &usage_records(&env.rt, 1).await[0];
    assert_eq!(
        (record["failed"].clone(), tokens(record)),
        (json!(false), (18, 22, 40)),
        "{record}"
    );
    // Complete payloads without newlines, then a failure.
    script(
        json!([
            "data: {\"choices\":[{\"delta\":{\"content\":\"Hello\"}}]}",
            "data: {\"choices\":[],\"usage\":{\"prompt_tokens\":11,\"completion_tokens\":22,\"total_tokens\":33}}"
        ]),
        "mid-stream chunk failure",
    );
    let (got, body) = env.chat_stream("acme-1").await;
    assert_eq!(got, 200, "{body}");
    let record = &usage_records(&env.rt, 1).await[0];
    assert_eq!(
        (record["failed"].clone(), tokens(record)),
        (json!(true), (11, 22, 33)),
        "{record}"
    );
    assert_eq!(record["fail"]["body"], "mid-stream chunk failure", "{record}");
    env.finish().await;
}

/// Go `NewExecutorUsageReporter(ctx, a, …)` for a credential-bound plugin attempt whose
/// formats differ from the client's: exactly one record per attempt, typed as the
/// host's `executorAdapter` for the plugin's provider, with usage read in the plugin's
/// Claude output (the client's chat format misses its cache reads) and the reasoning effort
/// of the translated payload (`max` becomes a 128000-token budget, which reads back as
/// `xhigh`). A stream the client drops publishes one 499 "context canceled" record
/// keeping the usage seen so far, and a request that fails before the plugin is called
/// (no translator into its input format) one failure record of the same type. Counting tokens and the model router's credential-less
/// attempts create no reporter, so they publish nothing.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_plugin_executor_records_each_attempt_once_in_its_own_format() {
    let Some(env) = env(
        "plugin-owned-usage-formats",
        &[
            ("catalog", "model_provider"),
            ("acme", "executor\n      inputs: claude\n      outputs: claude"),
            ("rt", "model_router,executor"),
            ("privcat", "model_provider"),
            ("priv", "executor\n      inputs: private-v1\n      outputs: private-v1"),
        ],
        USAGE_ON,
        &[("acme.json", r#"{"type":"acme"}"#), ("priv.json", r#"{"type":"priv"}"#)],
        |records| {
            canned(
                records,
                "catalog",
                "model.static",
                json!({"Provider":"acme","Models":[{"ID":"acme-1"}]}),
            );
            canned(
                records,
                "privcat",
                "model.static",
                json!({"Provider":"priv","Models":[{"ID":"priv-1"}]}),
            );
        },
    )
    .await
    else {
        return;
    };
    env.rt.usage_queue().configure(true, &env.rt.config());
    let no_more_records = || async {
        tokio::time::sleep(Duration::from_millis(200)).await;
        let extra = env.rt.usage_queue().pop_oldest(10);
        assert!(
            extra.is_empty(),
            "{:?}",
            extra.iter().map(|r| String::from_utf8_lossy(r)).collect::<Vec<_>>()
        );
    };
    let encode = |s: &str| base64::Engine::encode(&base64::engine::general_purpose::STANDARD, s);
    let tokens = |record: &Value| {
        let t = &record["tokens"];
        (
            t["input_tokens"].as_i64().unwrap(),
            t["output_tokens"].as_i64().unwrap(),
            t["cache_read_tokens"].as_i64().unwrap(),
        )
    };

    // Buffered.
    canned(
        &env.records,
        "acme",
        "executor.execute",
        json!({"Payload": encode(
            r#"{"id":"msg_1","type":"message","role":"assistant","model":"acme-1","content":[{"type":"text","text":"hi"}],"stop_reason":"end_turn","usage":{"input_tokens":34,"output_tokens":499,"cache_read_input_tokens":6}}"#
        )}),
    );
    let (got, body) = env
        .post(r#"{"model":"acme-1","reasoning_effort":"max","messages":[{"role":"user","content":"hi"}]}"#.into())
        .await;
    assert_eq!(got, 200, "{body}");
    let sent = calls(&env.records, "acme", "executor.execute");
    let payload = base64::Engine::decode(
        &base64::engine::general_purpose::STANDARD,
        sent[0]["Payload"].as_str().unwrap(),
    )
    .unwrap();
    let payload: Value = serde_json::from_slice(&payload).unwrap();
    assert_eq!(payload["thinking"]["budget_tokens"], 128000, "{payload}");
    let record = &usage_records(&env.rt, 1).await[0];
    no_more_records().await;
    assert_eq!(
        (
            &record["provider"],
            &record["executor_type"],
            &record["reasoning_effort"]
        ),
        (&json!("acme"), &json!("executorAdapter"), &json!("xhigh")),
        "{record}"
    );
    assert_eq!(
        (record["failed"].clone(), tokens(record)),
        (json!(false), (34, 499, 6)),
        "{record}"
    );

    // A stream the client drops while the plugin holds it open. Each chunk is one
    // `data:` line, as Go's Claude translator reads a chunk.
    let dir = env.records.join("respond").join("acme");
    std::fs::write(dir.join("executor.execute_stream.close.hold"), "").unwrap();
    std::fs::write(
        dir.join("executor.execute_stream.script.once.json"),
        json!({"chunks": [
            "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_2\",\"type\":\"message\",\"role\":\"assistant\",\"model\":\"acme-1\",\"content\":[],\"usage\":{\"input_tokens\":21,\"output_tokens\":0,\"cache_read_input_tokens\":4}}}\n",
            "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n",
            "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"hi\"}}\n",
            "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":null},\"usage\":{\"input_tokens\":21,\"output_tokens\":9,\"cache_read_input_tokens\":4}}\n"
        ], "error": ""})
        .to_string(),
    )
    .unwrap();
    {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let body = r#"{"model":"acme-1","stream":true,"messages":[{"role":"user","content":"hi"}]}"#;
        let mut conn = tokio::net::TcpStream::connect(env.proxy).await.unwrap();
        let head = format!(
            "POST /v1/chat/completions HTTP/1.1\r\nHost: {}\r\nContent-Type: application/json\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{body}",
            env.proxy,
            body.len()
        );
        conn.write_all(head.as_bytes()).await.unwrap();
        let mut seen = Vec::new();
        let mut buf = [0u8; 4096];
        while !String::from_utf8_lossy(&seen).contains("data:") {
            let n = tokio::time::timeout(Duration::from_secs(5), conn.read(&mut buf))
                .await
                .unwrap_or_else(|_| panic!("no first event: {}", String::from_utf8_lossy(&seen)))
                .unwrap();
            assert!(n > 0, "stream ended early: {}", String::from_utf8_lossy(&seen));
            seen.extend_from_slice(&buf[..n]);
        }
        // Both chunks are emitted before the hold; give the second one time to land.
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    let record = &usage_records(&env.rt, 1).await[0];
    std::fs::remove_file(dir.join("executor.execute_stream.close.hold")).unwrap();
    no_more_records().await;
    assert_eq!(
        (
            record["failed"].clone(),
            record["fail"]["status_code"].clone(),
            record["fail"]["body"].clone(),
            tokens(record)
        ),
        (json!(true), json!(499), json!("context canceled"), (21, 9, 4)),
        "{record}"
    );
    assert_eq!(record["executor_type"], "executorAdapter", "{record}");

    // Counting tokens.
    canned(
        &env.records,
        "acme",
        "executor.count_tokens",
        json!({"Payload": encode(r#"{"input_tokens":5}"#)}),
    );
    let response = wreq::Client::new()
        .post(format!("http://{}/v1/messages/count_tokens", env.proxy))
        .header("content-type", "application/json")
        .body(r#"{"model":"acme-1","messages":[{"role":"user","content":"hi"}]}"#)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status().as_u16(), 200, "{}", response.text().await.unwrap());
    assert_eq!(calls(&env.records, "acme", "executor.count_tokens").len(), 1);
    no_more_records().await;

    // A request the plugin cannot take (no translator to `private-v1`) fails before
    // the plugin is called, still as the plugin executor's attempt.
    let (got, body) = env.chat_stream("priv-1").await;
    assert_ne!(got, 200, "{body}");
    assert!(calls(&env.records, "priv", "executor.execute_stream").is_empty());
    let record = &usage_records(&env.rt, 1).await[0];
    no_more_records().await;
    assert_eq!(
        (&record["failed"], &record["provider"], &record["executor_type"]),
        (&json!(true), &json!("priv"), &json!("executorAdapter")),
        "{record}"
    );

    // The model router's own executor, without a credential.
    std::fs::create_dir_all(env.records.join("respond").join("rt")).unwrap();
    std::fs::write(
        env.records.join("respond").join("rt").join("model.route.once.json"),
        json!({"ok":true,"result":{"Handled":true,"TargetKind":"self","Target":"rt","Reason":"test"}}).to_string(),
    )
    .unwrap();
    canned(
        &env.records,
        "rt",
        "executor.execute",
        json!({"Payload": encode(
            r#"{"id":"c1","object":"chat.completion","choices":[],"usage":{"prompt_tokens":3,"completion_tokens":4,"total_tokens":7}}"#
        )}),
    );
    let (got, body) = env.chat("routed-1").await;
    assert_eq!(got, 200, "{body}");
    assert_eq!(calls(&env.records, "rt", "executor.execute").len(), 1);
    no_more_records().await;
    env.finish().await;
}

/// Go's registry hooks on the plugin executor path: an executor that answers only in a
/// format no built-in translator knows is offered once a plugin translates responses,
/// and its answer reaches the client through that plugin's `response.translate`
/// (upstream format as `FromFormat`, the client's as `ToFormat`).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_plugin_response_translator_serves_a_private_executor_format() {
    let Some(env) = env(
        "plugin-owned-response-translator",
        &[
            ("catalog", "model_provider"),
            ("acme", "executor\n      inputs: openai\n      outputs: private-v1"),
            ("tr", "response_translator"),
        ],
        "",
        &[("acme.json", r#"{"type":"acme"}"#)],
        |records| {
            canned(
                records,
                "catalog",
                "model.static",
                json!({"Provider":"acme","Models":[{"ID":"acme-1"}]}),
            )
        },
    )
    .await
    else {
        return;
    };
    let encode = |s: &str| base64::Engine::encode(&base64::engine::general_purpose::STANDARD, s);
    canned(
        &env.records,
        "acme",
        "executor.execute",
        json!({"Payload": encode("private answer")}),
    );
    let translated = r#"{"id":"c1","object":"chat.completion","model":"acme-1","choices":[{"index":0,"message":{"role":"assistant","content":"translated"},"finish_reason":"stop"}]}"#;
    canned(
        &env.records,
        "tr",
        "response.translate",
        json!({"Body": encode(translated)}),
    );
    let (got, body) = env.chat("acme-1").await;
    assert_eq!(got, 200, "{body}");
    assert_eq!(
        serde_json::from_str::<Value>(&body).unwrap()["choices"][0]["message"]["content"],
        "translated",
        "{body}"
    );
    let asked = calls(&env.records, "tr", "response.translate");
    assert_eq!(asked.len(), 1, "{asked:?}");
    assert_eq!(
        (&asked[0]["FromFormat"], &asked[0]["ToFormat"]),
        (&json!("private-v1"), &json!("openai"))
    );
    let sent = base64::Engine::decode(
        &base64::engine::general_purpose::STANDARD,
        asked[0]["Body"].as_str().unwrap(),
    )
    .unwrap();
    assert_eq!(sent, b"private answer");

    // A stream: each plugin chunk is one translator call, framed for the chat client.
    let chunk = r#"{"id":"c1","object":"chat.completion.chunk","choices":[{"index":0,"delta":{"content":"hi"}}]}"#;
    canned(&env.records, "tr", "response.translate", json!({"Body": encode(chunk)}));
    let dir = env.records.join("respond").join("acme");
    std::fs::write(
        dir.join("executor.execute_stream.script.once.json"),
        json!({"chunks": ["private chunk"], "error": ""}).to_string(),
    )
    .unwrap();
    let (got, body) = env.chat_stream("acme-1").await;
    assert_eq!(got, 200, "{body}");
    assert!(body.starts_with(&format!("data: {chunk}\n\n")), "{body}");
    let asked = calls(&env.records, "tr", "response.translate");
    assert_eq!(asked.len(), 2, "{asked:?}");
    assert_eq!(asked[1]["Stream"], true);
    env.finish().await;
}

/// Go's OpenAI-compatible executor calls `TranslateStream` once per upstream frame
/// (`data: ` plus the joined payload), so plugin response hooks run once per frame. An
/// after normalizer that answers even an empty input exposes any extra call: a blank
/// line passed after each frame would add a second chunk.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn plugin_response_hooks_run_once_per_openai_compatible_stream_frame() {
    let upstream = {
        let sse = "data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\ndata: [DONE]\n\n";
        let handler = move || async move {
            (
                [(axum::http::header::CONTENT_TYPE, "text/event-stream")],
                sse.to_owned(),
            )
        };
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move {
            axum::serve(listener, axum::Router::new().fallback(handler))
                .await
                .unwrap()
        });
        url
    };
    let compat = format!(
        "openai-compatibility:\n  - name: up\n    base-url: {upstream}/v1\n    api-key-entries: [{{api-key: k1}}]\n    models: [{{name: up-model}}]\n"
    );
    let Some(env) = env(
        "plugin-owned-stream-hooks",
        &[("norm", "response_after_translator")],
        &compat,
        &[],
        |records| {
            // The finish reason keeps [DONE]: without one the stream ends with an error (Go 97dd9eec).
            let chunk = r#"{"plugin":true,"choices":[{"finish_reason":"stop"}]}"#;
            let body = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, chunk);
            canned(records, "norm", "response.normalize_after", json!({ "Body": body }));
        },
    )
    .await
    else {
        return;
    };
    let (got, body) = env.chat_stream("up-model").await;
    assert_eq!(got, 200, "{body}");
    assert_eq!(
        body,
        "data: {\"plugin\":true,\"choices\":[{\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n"
    );
    let asked = calls(&env.records, "norm", "response.normalize_after");
    assert_eq!(
        asked.len(),
        1,
        "one call for the content frame, none for [DONE]: {asked:?}"
    );
    env.finish().await;
}

/// Go's file synthesizer asks the plugin auth provider whose identifier is the file's
/// type before the built-in path. Credentials load before plugins start, so they load
/// again once plugins are up, before serving: here two credentials from one file,
/// served by the plugin executor. Of two auth providers with the same identifier,
/// only the first in priority order is asked; a type no plugin owns stays built-in.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn plugin_parsed_auth_files_load_through_the_first_matching_auth_provider() {
    let parsed = |id: &str, token: &str| json!({"Provider":"acme","ID":id,"Metadata":{"type":"acme","token":token}});
    let Some(env) = env(
        "plugin-owned-auth-parse",
        &[
            ("catalog", "model_provider"),
            ("acme", "auth_provider,executor"),
            ("other", "auth_provider"),
        ],
        "",
        &[
            ("acme.json", r#"{"type":"acme","wrapped":"material","priority":4}"#),
            ("claude.json", r#"{"type":"claude","access_token":"fake"}"#),
        ],
        |records| {
            canned(
                records,
                "catalog",
                "model.static",
                json!({"Provider":"acme","Models":[{"ID":"acme-1"}]}),
            );
            canned(
                records,
                "acme",
                "auth.parse",
                json!({"Handled":true,"Auths":[parsed("acme-a", "tok-a"), parsed("acme-b", "tok-b")]}),
            );
            canned(records, "other", "auth.identifier", json!({"identifier":"acme"}));
            canned(
                records,
                "other",
                "auth.parse",
                json!({"Handled":true,"Auths":[parsed("other-a", "tok-o")]}),
            );
        },
    )
    .await
    else {
        return;
    };
    let ids = |rt: &cpa_server::Runtime| {
        let mut ids: Vec<String> = rt.store().snapshot().iter().map(|c| c.id.clone()).collect();
        ids.sort();
        ids
    };
    assert_eq!(
        ids(&env.rt),
        ["acme.json", "claude.json"],
        "loaded before plugins start"
    );
    cpa_server::plugins::reload_plugin_auth_files(&env.rt).await;
    assert_eq!(ids(&env.rt), ["acme-a", "acme-b", "claude.json"]);
    let asked = calls(&env.records, "acme", "auth.parse");
    assert!(!asked.is_empty());
    for req in &asked {
        assert_eq!(req["Provider"], "acme", "only the file whose type it owns: {req}");
        assert!(req["Path"].as_str().unwrap().ends_with("acme.json"), "{req}");
    }
    assert!(
        calls(&env.records, "other", "auth.parse").is_empty(),
        "the second provider with the same identifier is never asked"
    );
    let store = env.rt.store();
    let a = store.get("acme-a").unwrap();
    assert_eq!(a.provider, "acme");
    assert_eq!(a.attributes["plugin_virtual"], "true");
    assert_eq!(a.attributes["priority"], "4", "the file's priority");
    assert_eq!(a.metadata["token"], "tok-a");
    let (status, body) = env.chat("acme-1").await;
    assert_eq!(status, 200, "{body}");
    let executed = calls(&env.records, "acme", "executor.execute");
    assert_eq!(executed.len(), 1);
    let auth = executed[0]["AuthID"].as_str().unwrap();
    let token = if auth == "acme-a" { "tok-a" } else { "tok-b" };
    assert!(["acme-a", "acme-b"].contains(&auth), "{auth}");
    assert_eq!(executed[0]["AuthMetadata"]["token"], token);
    // Go `Manager.persist` skips plugin-virtual auths: the result never rewrites the
    // shared source file with one credential's document.
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(
        std::fs::read_to_string(env.rt.config().auth_dir.join("acme.json")).unwrap(),
        r#"{"type":"acme","wrapped":"material","priority":4}"#
    );
    env.finish().await;
}

/// Waits up to 10 seconds for `done`.
async fn eventually(what: &str, done: impl Fn() -> bool) {
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while !done() {
        assert!(std::time::Instant::now() < deadline, "{what}");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// Removes a recorder hold file when dropped, so a failed assertion never leaves a
/// plugin call waiting.
struct Release(PathBuf);

impl Drop for Release {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// Releases a recorder hold created at `since`, first checking that it still holds
/// `pending`: the recorder answers by itself after 10 s, and an overlap it let go of
/// would test nothing.
fn release<T>(hold: Release, since: std::time::Instant, pending: &tokio::task::JoinHandle<T>) {
    assert!(since.elapsed() < Duration::from_secs(8), "the hold may have expired");
    assert!(!pending.is_finished(), "released before the hold");
    drop(hold);
}

/// Asserts the saved file and its registered credential hold the same document.
fn file_matches_store(env: &Env, name: &str) {
    let on_disk: Value = serde_json::from_slice(&std::fs::read(env.rt.config().auth_dir.join(name)).unwrap()).unwrap();
    let stored = env.rt.store().get(name).unwrap();
    assert_eq!(on_disk, Value::Object(stored.metadata.clone()), "{name}");
}

/// As in Go, an auth provider can save a credential (`host.auth.save`) while it parses
/// a file for a reload: plugins parse before the reload takes the management disk lock
/// the save takes. The save lands, the reload completes, and the saved file and its
/// credential agree.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_auth_provider_can_save_while_it_parses() {
    let Some(env) = env(
        "plugin-owned-auth-parse-save",
        &[("acme", "auth_provider")],
        "",
        &[("acme.json", r#"{"type":"acme","wrapped":"material"}"#)],
        |records| {
            // The recorder makes the envelope's `host_calls` before it answers.
            let dir = records.join("respond").join("acme");
            std::fs::create_dir_all(&dir).unwrap();
            let envelope = json!({
                "ok": true,
                "result": {"Handled": true, "Auths": [{"Provider":"acme","ID":"acme-a","Metadata":{"type":"acme"}}]},
                "host_calls": [{
                    "method": "host.auth.save",
                    "request": {"name": "saved.json", "json": {"type": "claude", "access_token": "fake"}}
                }]
            });
            std::fs::write(dir.join("auth.parse.json"), envelope.to_string()).unwrap();
        },
    )
    .await
    else {
        return;
    };
    let path = env.dir.join("config.yaml");
    std::fs::write(&path, &env.yaml).unwrap();
    let management = cpa_server::management::Management::new(env.rt.clone(), path);
    // The watcher's first observation reloads the auth folder.
    let watcher = cpa_server::watching::start(&management);
    eventually("the reload stalled", || env.rt.store().get("acme-a").is_some()).await;
    let saved = calls(&env.records, "acme", "host_callback");
    assert!(!saved.is_empty());
    for response in &saved {
        assert_eq!(response["ok"], true, "{response}");
    }
    eventually("the saved credential was never registered", || {
        env.rt.store().get("saved.json").is_some()
    })
    .await;
    file_matches_store(&env, "saved.json");
    assert_eq!(
        env.rt.store().get("saved.json").unwrap().metadata["access_token"],
        "fake"
    );
    watcher.abort();
    drop(management);
    env.finish().await;
}

/// The lock inversion a reload that parsed under the disk lock would deadlock on: a
/// plugin that answers one call at a time (`serial`) is saving a credential, holding
/// its turn while the save waits for the disk lock, when a reload, held on an earlier
/// file's parse, reaches that plugin's file. The reload parses before it takes the
/// lock, so the save lands, then the parse, and both credentials are published.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_reload_and_a_serial_plugins_save_both_complete() {
    let auth = |id: &str| json!({"Handled":true,"Auths":[{"Provider":"acme","ID":id,"Metadata":{"type":"acme"}}]});
    let Some(env) = env(
        "plugin-owned-auth-parse-inversion",
        &[
            ("slow", "auth_provider"),
            ("acme", "auth_provider,management_api\n      serial: true"),
        ],
        "",
        &[
            ("a-slow.json", r#"{"type":"slow"}"#),
            ("z-acme.json", r#"{"type":"acme"}"#),
        ],
        |records| {
            canned(records, "slow", "auth.parse", auth("slow-a"));
            canned(records, "acme", "auth.parse", auth("acme-a"));
            std::fs::write(records.join("respond/slow/auth.parse.hold"), "").unwrap();
        },
    )
    .await
    else {
        return;
    };
    let release = Release(env.records.join("respond/slow/auth.parse.hold"));
    let path = env.dir.join("config.yaml");
    std::fs::write(&path, &env.yaml).unwrap();
    let management = cpa_server::management::Management::new(env.rt.clone(), path);
    let watcher = cpa_server::watching::start(&management);
    eventually("the reload never reached the first file", || {
        !calls(&env.records, "slow", "auth.parse").is_empty()
    })
    .await;
    // acme takes its turn and saves; the save needs the disk lock.
    let body = json!({"calls":[{"method":"host.auth.save","request":{
        "name":"saved.json","json":{"type":"claude","access_token":"fake"}}}]});
    let saving = tokio::spawn({
        let host = env.rt.plugins().clone();
        async move {
            host.serve_management(cpa_plugin::management::Inbound {
                method: "POST".into(),
                path: "/v0/management/rec/acme/calls".into(),
                body: serde_json::to_vec(&body).unwrap().into(),
                ..Default::default()
            })
            .await
        }
    });
    eventually("acme never took the save", || {
        !calls(&env.records, "acme", "management.handle").is_empty()
    })
    .await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    // The reload goes on to acme's file, whose parse needs acme's turn.
    drop(release);
    let response = tokio::time::timeout(Duration::from_secs(10), saving)
        .await
        .expect("the save stalled")
        .unwrap()
        .unwrap();
    let response: Value = serde_json::from_slice(&response.body).unwrap();
    let result = response["results"][0].as_str().unwrap();
    assert!(result.starts_with("0 "), "{result}");
    eventually("the reload stalled", || {
        let store = env.rt.store();
        ["slow-a", "acme-a", "saved.json"]
            .iter()
            .all(|id| store.get(id).is_some())
    })
    .await;
    file_matches_store(&env, "saved.json");
    watcher.abort();
    drop(management);
    env.finish().await;
}

/// Plugin `label` saves auth file `name` with `json` (`host.auth.save` through its
/// management route); returns the host's response envelope.
async fn host_save(env: &Env, label: &str, name: &str, json: Value) -> Value {
    let body = json!({"calls":[{"method":"host.auth.save","request":{"name":name,"json":json}}]});
    let response = env
        .rt
        .plugins()
        .serve_management(cpa_plugin::management::Inbound {
            method: "POST".into(),
            path: format!("/v0/management/rec/{label}/calls"),
            body: serde_json::to_vec(&body).unwrap().into(),
            ..Default::default()
        })
        .await
        .unwrap();
    let response: Value = serde_json::from_slice(&response.body).unwrap();
    let result = response["results"][0].as_str().unwrap().to_owned();
    serde_json::from_str::<Value>(result.split_once(' ').unwrap().1).unwrap()
}

/// What a save registers and writes follows Go's `buildAuthsFromFileData` and
/// `Manager.persist`: the parsed auth's own fields win over the file's in either
/// spelling, the file's priority overrides the parsed one, a runtime-only or
/// plugin-virtual auth or one whose storage cannot be built leaves the saved bytes in
/// the file, and an auth naming another file is refused before the write.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_plugin_save_applies_the_files_settings_and_persists_as_go_does() {
    let Some(env) = env(
        "plugin-owned-auth-save-settings",
        &[("acme", "auth_provider,management_api")],
        "",
        &[],
        |_| {},
    )
    .await
    else {
        return;
    };
    let path = env.dir.join("config.yaml");
    std::fs::write(&path, &env.yaml).unwrap();
    let management = cpa_server::management::Management::new(env.rt.clone(), path);
    let store = env.rt.store().clone();
    let auth_dir = env.rt.config().auth_dir.clone();
    let answer = |auth: Value| {
        canned(
            &env.records,
            "acme",
            "auth.parse",
            json!({"Handled":true,"Auths":[auth]}),
        )
    };
    let on_disk = |name: &str| std::fs::read(auth_dir.join(name)).unwrap();

    // The plugin's `base-url` is normalized before the file's `base_url` is inherited.
    answer(json!({"Provider":"acme","ID":"a","Metadata":{"type":"acme","base-url":"https://plugin.invalid"}}));
    let saved = host_save(
        &env,
        "acme",
        "a.json",
        json!({"type":"acme","base_url":"https://file.invalid"}),
    )
    .await;
    assert_eq!(saved["ok"], true, "{saved}");
    let a = store.get("a").unwrap();
    assert_eq!(a.metadata["base_url"], "https://plugin.invalid");
    assert!(a.metadata.get("base-url").is_none());

    // The file's priority, not the plugin's, is the credential's and the file's.
    answer(json!({"Provider":"acme","ID":"p","Metadata":{"type":"acme","priority":1}}));
    assert_eq!(
        host_save(&env, "acme", "p.json", json!({"type":"acme","priority":10})).await["ok"],
        true
    );
    let p = store.get("p").unwrap();
    assert_eq!(
        (p.attributes["priority"].as_str(), p.metadata["priority"].as_f64()),
        ("10", Some(10.0))
    );
    // Go's encoder writes the decoded float 10 as `10`.
    let written = String::from_utf8(on_disk("p.json")).unwrap();
    assert!(
        written.contains(r#""priority":10"#) && !written.contains("10.0"),
        "{written}"
    );

    // Runtime-only and plugin-virtual auths are registered, and the file keeps the
    // saved bytes; so does one whose storage JSON is not an object ("[]").
    for (name, attributes, storage) in [
        ("r.json", json!({"runtime_only":"true"}), Value::Null),
        ("v.json", json!({"plugin_virtual":"TRUE"}), Value::Null),
        ("s.json", json!({}), json!("W10=")),
    ] {
        let id = name.trim_end_matches(".json");
        answer(
            json!({"Provider":"acme","ID":id,"Attributes":attributes,"StorageJSON":storage,
            "Metadata":{"type":"acme","access_token":"parsed"}}),
        );
        let submitted = json!({"type":"acme","note":"as saved"});
        let saved = host_save(&env, "acme", name, submitted.clone()).await;
        assert_eq!(saved["ok"], true, "{name}: {saved}");
        let written: Value = serde_json::from_slice(&on_disk(name)).unwrap();
        assert_eq!(written, submitted, "{name}");
        assert_eq!(store.get(id).unwrap().metadata["access_token"], "parsed", "{name}");
    }

    // Persisting records the auth's disabled state in its metadata before the storage
    // fails to serialize (Go `FileTokenStore.Save`), and the file keeps the saved bytes.
    answer(json!({"Provider":"acme","ID":"d","Disabled":true,"StorageJSON":"W10=",
        "Metadata":{"type":"acme","disabled":false}}));
    let submitted = json!({"type":"acme","disabled":false});
    assert_eq!(host_save(&env, "acme", "d.json", submitted.clone()).await["ok"], true);
    let d = store.get("d").unwrap();
    assert!(d.disabled);
    assert_eq!(d.metadata["disabled"], true);
    assert_eq!(serde_json::from_slice::<Value>(&on_disk("d.json")).unwrap(), submitted);
    for id in ["r", "v"] {
        assert!(store.get(id).unwrap().metadata.get("disabled").is_none(), "{id}");
    }

    // Only Go's save-time rules apply: the file's excluded models do not replace the
    // parsed auth's (the watcher applies them when it next parses the file).
    answer(
        json!({"Provider":"acme","ID":"x","Attributes":{"excluded_models":"plugin-model"},
        "Metadata":{"type":"acme","excluded_models":["plugin-model"]}}),
    );
    let saved = host_save(
        &env,
        "acme",
        "x.json",
        json!({"type":"acme","excluded_models":["file-model"]}),
    )
    .await;
    assert_eq!(saved["ok"], true, "{saved}");
    let x = store.get("x").unwrap();
    assert_eq!(x.attributes["excluded_models"], "plugin-model");
    assert_eq!(x.metadata["excluded_models"], json!(["plugin-model"]));

    // JSON null decodes as Go decodes it into a map (no fields): the auth provider that
    // handles it is registered, and its document (the null storage overlaid with its
    // metadata) is persisted.
    answer(json!({"Provider":"acme","ID":"n","Metadata":{"type":"acme"}}));
    let saved = host_save(&env, "acme", "n.json", Value::Null).await;
    assert_eq!(saved["ok"], true, "{saved}");
    assert!(store.get("n").is_some());
    assert_eq!(
        serde_json::from_slice::<Value>(&on_disk("n.json")).unwrap(),
        json!({"type":"acme","disabled":false})
    );

    // An auth naming another file is refused, and nothing is written.
    answer(json!({"Provider":"acme","ID":"l","FileName":"logical.json","Metadata":{"type":"acme"}}));
    let refused = host_save(&env, "acme", "physical.json", json!({"type":"acme"})).await;
    assert_eq!(refused["ok"], false, "{refused}");
    assert!(
        refused["error"]["message"].as_str().unwrap().contains("logical.json"),
        "{refused}"
    );
    assert!(!auth_dir.join("physical.json").exists());
    assert!(store.get("l").is_none());
    drop(management);
    env.finish().await;
}

/// `host.auth.save` registers what the plugin auth provider for the file's type parses,
/// as Go's `buildAuthsFromFileData` does: the file's earlier credentials go, one parsed
/// auth inherits the file's other fields and, without an ID of its own, keeps the ID of
/// the file's credential (as a built-in save does), and a failed parse fails the save
/// before anything is written.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_plugin_save_registers_what_the_auth_provider_parses() {
    let Some(env) = env(
        "plugin-owned-auth-save-parse",
        &[("acme", "auth_provider,management_api")],
        "",
        &[("pair.json", r#"{"type":"acme","v":0}"#)],
        |records| {
            canned(
                records,
                "acme",
                "auth.parse",
                json!({"Handled":true,"Auths":[
                    {"Provider":"acme","ID":"acme-1","Metadata":{"type":"acme"}},
                    {"Provider":"acme","ID":"acme-2","Metadata":{"type":"acme"}}]}),
            );
        },
    )
    .await
    else {
        return;
    };
    let path = env.dir.join("config.yaml");
    std::fs::write(&path, &env.yaml).unwrap();
    let management = cpa_server::management::Management::new(env.rt.clone(), path);
    cpa_server::plugins::reload_plugin_auth_files(&env.rt).await;
    let store = env.rt.store().clone();
    assert!(store.get("acme-1").is_some() && store.get("acme-2").is_some());
    let save = |name: &'static str, json: Value| host_save(&env, "acme", name, json);
    let answer = |auth: Value| {
        canned(
            &env.records,
            "acme",
            "auth.parse",
            json!({"Handled":true,"Auths":[auth]}),
        )
    };

    // One auth without an ID: the file's two virtual credentials go, and the auth takes
    // the file's ID and its other fields.
    answer(json!({"Provider":"acme","Metadata":{"type":"acme","access_token":"t1"}}));
    let saved = save("pair.json", json!({"type":"acme","note":"kept","v":1})).await;
    assert_eq!(saved["ok"], true, "{saved}");
    let asked = calls(&env.records, "acme", "auth.parse");
    let asked = asked.last().unwrap();
    assert_eq!(asked["Provider"], "acme");
    assert_eq!(asked["FileName"], "pair.json");
    assert_eq!(
        asked["Path"],
        env.rt.config().auth_dir.join("pair.json").display().to_string()
    );
    assert!(
        store.get("acme-1").is_none() && store.get("acme-2").is_none(),
        "stale credentials stayed"
    );
    let one = store.get("pair.json").expect("the parsed auth was not registered");
    assert_eq!(one.metadata["access_token"], "t1");
    assert_eq!(one.metadata["note"], "kept");
    file_matches_store(&env, "pair.json");

    // An auth with its own ID replaces the file's credential.
    answer(json!({"Provider":"acme","ID":"acct-7","Metadata":{"type":"acme","access_token":"t2"}}));
    assert_eq!(save("pair.json", json!({"type":"acme","v":2})).await["ok"], true);
    assert!(store.get("pair.json").is_none());
    assert_eq!(store.get("acct-7").unwrap().metadata["access_token"], "t2");

    // Without an ID again, the auth keeps the file's credential's ID.
    answer(json!({"Provider":"acme","Metadata":{"type":"acme","access_token":"t3"}}));
    assert_eq!(save("pair.json", json!({"type":"acme","v":3})).await["ok"], true);
    assert!(store.get("pair.json").is_none());
    assert_eq!(store.get("acct-7").unwrap().metadata["access_token"], "t3");

    // A built-in save of the file keeps that ID too.
    assert_eq!(
        save("pair.json", json!({"type":"claude","access_token":"t4"})).await["ok"],
        true
    );
    assert!(store.get("pair.json").is_none());
    let built_in = store.get("acct-7").unwrap();
    assert_eq!(
        (built_in.provider.as_str(), &built_in.metadata["access_token"]),
        ("claude", &json!("t4"))
    );

    // A failed parse fails the save, and nothing is written.
    std::fs::write(
        env.records.join("respond/acme/auth.parse.json"),
        json!({"ok":false,"error":{"code":"bad","message":"unreadable material"}}).to_string(),
    )
    .unwrap();
    let failed = save("bad.json", json!({"type":"acme"})).await;
    assert_eq!(failed["ok"], false, "{failed}");
    let message = failed["error"]["message"].as_str().unwrap();
    assert!(
        message.contains("invalid auth file") && message.contains("unreadable material"),
        "{message}"
    );
    assert!(!env.rt.config().auth_dir.join("bad.json").exists());
    assert!(store.get("bad.json").is_none());
    drop(management);
    env.finish().await;
}

/// Go registers a new auth from a save with the `NextRefreshAfter` its plugin parsed: a
/// saved credential the parse dates to 2099 waits, while one saved without a date is due
/// by its interval at once.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_saved_auth_waits_for_its_parsed_refresh_deadline() {
    let Some(env) = env(
        "plugin-owned-auth-save-deadline",
        &[("acme", "auth_provider,management_api")],
        "",
        &[],
        |records| {
            canned(
                records,
                "acme",
                "auth.refresh",
                json!({"Auth":{"Metadata":{"type":"acme","access_token":"r","refresh_interval":"1h"}}}),
            );
        },
    )
    .await
    else {
        return;
    };
    let answer = |id: &str, deadline: Option<&str>| {
        let mut auth = json!({"Provider":"acme","ID":id,
            "Metadata":{"type":"acme","access_token":"t","refresh_interval":"1h"}});
        if let Some(at) = deadline {
            auth["NextRefreshAfter"] = at.into();
        }
        canned(
            &env.records,
            "acme",
            "auth.parse",
            json!({"Handled":true,"Auths":[auth]}),
        )
    };
    let path = env.dir.join("config.yaml");
    std::fs::write(&path, &env.yaml).unwrap();
    let management = cpa_server::management::Management::new(env.rt.clone(), path);
    answer("later", Some("2099-01-01T00:00:00Z"));
    let saved = host_save(&env, "acme", "later.json", json!({"type":"acme"})).await;
    assert_eq!(saved["ok"], true, "{saved}");
    answer("now", None);
    let saved = host_save(&env, "acme", "now.json", json!({"type":"acme"})).await;
    assert_eq!(saved["ok"], true, "{saved}");
    let store = env.rt.store();
    assert!(store.get("later").is_some() && store.get("now").is_some());
    let refreshed = |id: &str| {
        calls(&env.records, "acme", "auth.refresh")
            .iter()
            .filter(|r| r["AuthID"] == id)
            .count()
    };
    env.rt.start_auto_refresh();
    eventually("the undated credential's refresh", || refreshed("now") >= 1).await;
    tokio::time::sleep(Duration::from_millis(1500)).await;
    env.rt.stop_auto_refresh();
    assert_eq!(refreshed("later"), 0, "waiting for the parsed NextRefreshAfter");
    drop(management);
    env.finish().await;
}

/// A save registers its refresh deadlines with its credentials (Go
/// `preserveExistingAuthState`, then `upsertAuthRecordsForPath`): a save without an auth
/// manager or one refused after its parse leaves nothing for a later save of that ID; an
/// auth replacing the file's credential under a new ID takes that one's deadlines, unset
/// ones too, over its parsed one, and the one it replaced loses them; and a save
/// replacing an auth keeps its deadline.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_saved_auth_takes_its_refresh_deadline_only_when_registered() {
    let Some(env) = env(
        "plugin-owned-auth-save-deadline-registered",
        &[("acme", "auth_provider,management_api")],
        "",
        &[],
        |records| {
            canned(
                records,
                "acme",
                "auth.refresh",
                json!({"Auth":{"Metadata":{"type":"acme","access_token":"r","refresh_interval":"1h"}}}),
            );
        },
    )
    .await
    else {
        return;
    };
    let auth = |id: &str, deadline: Option<&str>| {
        let mut auth = json!({"Provider":"acme","ID":id,
            "Metadata":{"type":"acme","access_token":"t","refresh_interval":"1h"}});
        if let Some(at) = deadline {
            auth["NextRefreshAfter"] = at.into();
        }
        auth
    };
    let answer = |auths: Vec<Value>| {
        canned(
            &env.records,
            "acme",
            "auth.parse",
            json!({"Handled":true,"Auths":auths}),
        )
    };
    let save = |name: &'static str| host_save(&env, "acme", name, json!({"type":"acme"}));
    let later = Some("2099-01-01T00:00:00Z");

    // No auth manager: the file is written and nothing is registered.
    answer(vec![auth("orphan", later)]);
    assert_eq!(save("orphan.json").await["ok"], true);
    let path = env.dir.join("config.yaml");
    std::fs::write(&path, &env.yaml).unwrap();
    let management = cpa_server::management::Management::new(env.rt.clone(), path);
    answer(vec![auth("orphan", None)]);
    assert_eq!(save("orphan.json").await["ok"], true);

    // The second auth names another file, which refuses the save after the first parsed.
    answer(vec![
        auth("refused", later),
        json!({"Provider":"acme","ID":"refused-2","FileName":"elsewhere.json","Metadata":{"type":"acme"}}),
    ]);
    assert_eq!(save("refused.json").await["ok"], false);
    answer(vec![auth("refused", None)]);
    assert_eq!(save("refused.json").await["ok"], true);

    // A new ID at the same path takes the file's credential's deadlines.
    answer(vec![auth("old", later)]);
    assert_eq!(save("moved.json").await["ok"], true);
    answer(vec![auth("new", None)]);
    assert_eq!(save("moved.json").await["ok"], true);
    answer(vec![auth("plain", None)]);
    assert_eq!(save("swapped.json").await["ok"], true);
    answer(vec![auth("swapped", later)]);
    assert_eq!(save("swapped.json").await["ok"], true);

    // The same ID again keeps its own.
    answer(vec![auth("again", later)]);
    assert_eq!(save("again.json").await["ok"], true);
    answer(vec![auth("again", None)]);
    assert_eq!(save("again.json").await["ok"], true);

    // A credential a save removes takes its deadlines along (Go `Manager.Remove`): the
    // built-in credential of another file registered under its ID starts without them.
    answer(vec![auth("reused.json", later)]);
    assert_eq!(save("first.json").await["ok"], true);
    answer(vec![auth("moved-on", None)]);
    assert_eq!(save("first.json").await["ok"], true);
    canned(&env.records, "acme", "auth.parse", json!({"Handled":false}));
    let built_in = json!({"type":"acme","access_token":"t","refresh_interval":"1h"});
    assert_eq!(host_save(&env, "acme", "reused.json", built_in).await["ok"], true);

    let store = env.rt.store();
    for id in [
        "orphan",
        "refused",
        "new",
        "swapped",
        "again",
        "moved-on",
        "reused.json",
    ] {
        assert!(store.get(id).is_some(), "{id} was not registered");
    }
    assert!(store.get("old").is_none() && store.get("plain").is_none());
    let refreshed = |id: &str| {
        calls(&env.records, "acme", "auth.refresh")
            .iter()
            .filter(|r| r["AuthID"] == id)
            .count()
    };
    let (due, waiting) = (
        ["orphan", "refused", "swapped", "reused.json"],
        ["new", "again", "moved-on"],
    );
    env.rt.start_auto_refresh();
    // Bounded, so a credential that never comes due is reported with the others.
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while !due.iter().all(|id| refreshed(id) >= 1) && std::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    tokio::time::sleep(Duration::from_millis(1500)).await;
    env.rt.stop_auto_refresh();
    let wrong: Vec<_> = due
        .iter()
        .filter(|id| refreshed(id) == 0)
        .chain(waiting.iter().filter(|id| refreshed(id) != 0))
        .collect();
    assert!(wrong.is_empty(), "wrong refresh deadlines: {wrong:?}");
    drop(management);
    env.finish().await;
}

/// A reload that parses a saved file again keeps the saved auth's deadline (Go keeps the
/// registered auth's `NextRefreshAfter` when it registers it again while both are
/// enabled): the parsed one counts only for a registration that takes it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_reload_keeps_a_saved_auths_refresh_deadline() {
    let Some(env) = env(
        "plugin-owned-auth-save-deadline-reload",
        &[("acme", "auth_provider,management_api"), ("pp", "auth_provider")],
        "",
        &[("p.json", r#"{"type":"pp"}"#)],
        |records| {
            for label in ["acme", "pp"] {
                canned(
                    records,
                    label,
                    "auth.refresh",
                    json!({"Auth":{"Metadata":{"access_token":"r","refresh_interval":"1h"}}}),
                );
            }
            canned(
                records,
                "pp",
                "auth.parse",
                json!({"Handled":true,"Auths":[{"Provider":"pp","ID":"p-1",
                    "Metadata":{"type":"pp","access_token":"p","refresh_interval":"1h"}}]}),
            );
        },
    )
    .await
    else {
        return;
    };
    let answer = |deadline: Option<&str>| {
        let mut auth = json!({"Provider":"acme","ID":"kept",
            "Metadata":{"type":"acme","access_token":"t","refresh_interval":"1h"}});
        if let Some(at) = deadline {
            auth["NextRefreshAfter"] = at.into();
        }
        canned(
            &env.records,
            "acme",
            "auth.parse",
            json!({"Handled":true,"Auths":[auth]}),
        )
    };
    let path = env.dir.join("config.yaml");
    std::fs::write(&path, &env.yaml).unwrap();
    let management = cpa_server::management::Management::new(env.rt.clone(), path);
    answer(Some("2099-01-01T00:00:00Z"));
    let saved = host_save(&env, "acme", "kept.json", json!({"type":"acme"})).await;
    assert_eq!(saved["ok"], true, "{saved}");
    answer(None);
    cpa_server::plugins::reload_plugin_auth_files(&env.rt).await;
    let store = env.rt.store();
    assert!(store.get("kept").is_some() && store.get("p-1").is_some());
    let refreshed = |label: &str, id: &str| {
        calls(&env.records, label, "auth.refresh")
            .iter()
            .filter(|r| r["AuthID"] == id)
            .count()
    };
    env.rt.start_auto_refresh();
    eventually("the other plugin's refresh", || refreshed("pp", "p-1") >= 1).await;
    tokio::time::sleep(Duration::from_millis(1500)).await;
    env.rt.stop_auto_refresh();
    assert_eq!(refreshed("acme", "kept"), 0, "the saved NextRefreshAfter was lost");
    drop(management);
    env.finish().await;
}

/// A save the auth provider's parse of the saved file answers by saving that file again,
/// from inside the parse: Go parses the data before writing it, which recurses through
/// the plugin without bound. The inner save writes without parsing again, so the save
/// completes after one parse, the file holds what the outer save wrote, and the
/// parse's credential is registered.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_save_whose_parse_saves_the_same_file_completes_after_one_parse() {
    let Some(env) = env(
        "plugin-owned-auth-save-reentrant",
        &[("acme", "auth_provider,management_api")],
        "",
        &[],
        |records| {
            let dir = records.join("respond").join("acme");
            std::fs::create_dir_all(&dir).unwrap();
            let inner = json!({"type": "acme", "n": 2});
            std::fs::write(dir.join("auth.parse.json"), parse_saving("acme-a", inner)).unwrap();
        },
    )
    .await
    else {
        return;
    };
    let path = env.dir.join("config.yaml");
    std::fs::write(&path, &env.yaml).unwrap();
    let management = cpa_server::management::Management::new(env.rt.clone(), path);
    let body = json!({"calls":[{"method":"host.auth.save","request":{
        "name":"acme.json","json":{"type":"acme","n":1}}}]});
    let response = tokio::time::timeout(
        Duration::from_secs(10),
        env.rt.plugins().serve_management(cpa_plugin::management::Inbound {
            method: "POST".into(),
            path: "/v0/management/rec/acme/calls".into(),
            body: serde_json::to_vec(&body).unwrap().into(),
            ..Default::default()
        }),
    )
    .await
    .expect("the save never completed")
    .unwrap();
    let response: Value = serde_json::from_slice(&response.body).unwrap();
    let result = response["results"][0].as_str().unwrap();
    let saved: Value = serde_json::from_str(result.split_once(' ').unwrap().1).unwrap();
    assert_eq!(saved["ok"], true, "{saved}");
    assert_eq!(saved["result"]["name"], "acme.json");
    // The inner save also succeeded, with Go's response.
    let inner = calls(&env.records, "acme", "host_callback");
    assert_eq!(inner.len(), 1);
    assert_eq!(inner[0]["ok"], true, "{}", inner[0]);
    assert_eq!(inner[0]["result"]["name"], "acme.json");
    assert_eq!(calls(&env.records, "acme", "auth.parse").len(), 1);
    let on_disk: Value =
        serde_json::from_slice(&std::fs::read(env.rt.config().auth_dir.join("acme.json")).unwrap()).unwrap();
    assert_eq!((&on_disk["type"], &on_disk["n"]), (&json!("acme"), &json!(1)));
    assert!(env.rt.store().get("acme-a").is_some());
    drop(management);
    env.finish().await;
}

/// On a current-thread runtime the plugin call runs on a scoped thread while the
/// runtime waits, and a native plugin's synchronous callbacks arrive there: a save of
/// the file being parsed, made from inside the parse, is still the nested save, written
/// without a second parse (the watcher publishes the file on its next reload).
#[tokio::test(flavor = "current_thread")]
async fn a_nested_save_on_a_current_thread_runtime_skips_the_parse() {
    let Some(env) = env(
        "plugin-owned-auth-save-current-thread",
        &[("acme", "auth_provider")],
        "",
        &[("acme.json", r#"{"type":"acme"}"#)],
        |records| {
            let dir = records.join("respond").join("acme");
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(
                dir.join("auth.parse.json"),
                parse_saving("acme-a", json!({"type":"acme","n":1})),
            )
            .unwrap();
        },
    )
    .await
    else {
        return;
    };
    tokio::time::timeout(
        Duration::from_secs(10),
        cpa_server::plugins::reload_plugin_auth_files(&env.rt),
    )
    .await
    .expect("the reload never completed");
    let inner = calls(&env.records, "acme", "host_callback");
    assert_eq!(inner.len(), 1, "{inner:?}");
    assert_eq!(inner[0]["ok"], true, "{}", inner[0]);
    assert_eq!(calls(&env.records, "acme", "auth.parse").len(), 1, "no second parse");
    let on_disk: Value =
        serde_json::from_slice(&std::fs::read(env.rt.config().auth_dir.join("acme.json")).unwrap()).unwrap();
    assert_eq!(on_disk["n"], 1);
    env.finish().await;
}

/// The save guard is tied to the parsing thread, not just the plugin and file: while
/// the watcher's reload holds the plugin's parse of P, a save of P that the same plugin
/// makes for another request (on another thread) is parsed as in Go, so an invalid
/// weight fails the save and nothing is written.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_save_from_another_thread_during_a_parse_of_its_file_is_checked_as_in_go() {
    let Some(env) = env(
        "plugin-owned-auth-save-concurrent",
        &[("acme", "auth_provider,management_api")],
        "",
        &[("p.json", r#"{"type":"acme"}"#)],
        |records| {
            canned(
                records,
                "acme",
                "auth.parse",
                json!({"Handled":true,"Auths":[{"Provider":"acme","ID":"acme-a","Metadata":{"type":"acme"}}]}),
            );
            std::fs::write(records.join("respond/acme/auth.parse.hold"), "").unwrap();
        },
    )
    .await
    else {
        return;
    };
    let release = Release(env.records.join("respond/acme/auth.parse.hold"));
    let (management, watcher) = watch(&env);
    eventually("the reload never reached the parse", || {
        !calls(&env.records, "acme", "auth.parse").is_empty()
    })
    .await;
    let saved = host_save(&env, "acme", "p.json", json!({"type":"acme","weight":1.5})).await;
    assert_eq!(saved["ok"], false, "{saved}");
    assert!(
        saved["error"]["message"]
            .as_str()
            .unwrap()
            .contains("invalid auth weight"),
        "{saved}"
    );
    let on_disk = std::fs::read(env.rt.config().auth_dir.join("p.json")).unwrap();
    assert_eq!(on_disk, br#"{"type":"acme"}"#);
    drop(release);
    eventually("the reload stalled", || env.rt.store().get("acme-a").is_some()).await;
    watcher.abort();
    drop(management);
    env.finish().await;
}

/// A plugin whose parse saves the parsed file from a goroutine it spawns and waits for:
/// that save reaches the host on another thread. During a save's parse it is refused
/// and writes nothing, so the recursion Go has stops after one parse; during a reload's
/// parse it gets the save's own parse, as in Go, whose nested save is then refused.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_spawned_save_during_a_saves_parse_is_refused_and_the_recursion_stops() {
    let Some(env) = env(
        "plugin-owned-auth-save-spawned",
        &[("acme", "auth_provider,management_api")],
        "",
        &[],
        |records| {
            let dir = records.join("respond").join("acme");
            std::fs::create_dir_all(&dir).unwrap();
            let mut envelope: Value =
                serde_json::from_str(&parse_saving("acme-a", json!({"type":"acme","n":2}))).unwrap();
            envelope["spawn"] = true.into();
            std::fs::write(dir.join("auth.parse.json"), envelope.to_string()).unwrap();
        },
    )
    .await
    else {
        return;
    };
    let path = env.dir.join("config.yaml");
    std::fs::write(&path, &env.yaml).unwrap();
    let management = cpa_server::management::Management::new(env.rt.clone(), path);
    let file = env.rt.config().auth_dir.join("acme.json");
    let n = || serde_json::from_slice::<Value>(&std::fs::read(&file).unwrap()).unwrap()["n"].clone();
    let refused = |response: &Value| {
        response["ok"] == false
            && response["error"]["message"]
                .as_str()
                .is_some_and(|m| m.contains("being parsed"))
    };

    // A save: its parse's spawned save is refused.
    let saved = tokio::time::timeout(
        Duration::from_secs(10),
        host_save(&env, "acme", "acme.json", json!({"type":"acme","n":1})),
    )
    .await
    .expect("the save never completed");
    assert_eq!(saved["ok"], true, "{saved}");
    assert_eq!(calls(&env.records, "acme", "auth.parse").len(), 1);
    let inner = calls(&env.records, "acme", "host_callback");
    assert_eq!(inner.len(), 1);
    assert!(refused(&inner[0]), "{}", inner[0]);
    assert_eq!(n(), json!(1));
    assert!(env.rt.store().get("acme-a").is_some());

    // A reload's parse: the spawned save is parsed, and that parse's spawned save is
    // refused.
    tokio::time::timeout(
        Duration::from_secs(10),
        cpa_server::plugins::reload_plugin_auth_files(&env.rt),
    )
    .await
    .expect("the reload never completed");
    assert_eq!(calls(&env.records, "acme", "auth.parse").len(), 3);
    let inner = calls(&env.records, "acme", "host_callback");
    assert_eq!(inner.len(), 3);
    assert_eq!(inner.iter().filter(|r| refused(r)).count(), 2, "{inner:?}");
    assert!(inner.iter().any(|r| r["ok"] == true), "{inner:?}");
    assert_eq!(n(), json!(2));
    drop(management);
    env.finish().await;
}

/// A file that changes after plugins parsed it, before the reload's scan reads it, is
/// not parsed again by the scan (which may run under the disk lock): it keeps its
/// credentials until the next reload parses the new bytes. A file that appears then is
/// published only once parsed, as Go's watcher asks the plugin first.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_file_changed_after_its_parse_keeps_its_credentials_until_the_next_reload() {
    let auth = |provider: &str, id: &str| json!({"Handled":true,"Auths":[{"Provider":provider,"ID":id,"Metadata":{"type":provider}}]});
    let Some(env) = env(
        "plugin-owned-auth-parse-deferred",
        &[("acme", "auth_provider"), ("slow", "auth_provider")],
        "",
        &[
            ("0-acme.json", r#"{"type":"acme","v":1}"#),
            ("a-slow.json", r#"{"type":"slow"}"#),
        ],
        |records| {
            canned(records, "acme", "auth.parse", auth("acme", "acme-a"));
            canned(records, "slow", "auth.parse", auth("slow", "slow-a"));
        },
    )
    .await
    else {
        return;
    };
    cpa_server::plugins::reload_plugin_auth_files(&env.rt).await;
    assert!(env.rt.store().get("acme-a").is_some());
    let parsed = || calls(&env.records, "acme", "auth.parse").len();
    assert_eq!(parsed(), 1);
    let hold = env.records.join("respond/slow/auth.parse.hold");
    std::fs::write(&hold, "").unwrap();
    let release = Release(hold);
    let slow_parses = calls(&env.records, "slow", "auth.parse").len();
    // Changed, so the next reload asks about it again, and is held there.
    std::fs::write(env.rt.config().auth_dir.join("a-slow.json"), r#"{"type":"slow","v":2}"#).unwrap();
    let reload = tokio::spawn({
        let rt = env.rt.clone();
        async move { cpa_server::plugins::reload_plugin_auth_files(&rt).await }
    });
    eventually("the reload never reached the held parse", || {
        calls(&env.records, "slow", "auth.parse").len() > slow_parses
    })
    .await;
    // acme's file was parsed (cached) before the held one; it changes now.
    std::fs::write(env.rt.config().auth_dir.join("0-acme.json"), r#"{"type":"acme","v":2}"#).unwrap();
    std::fs::write(
        env.rt.config().auth_dir.join("1-new.json"),
        r#"{"type":"claude","access_token":"fake"}"#,
    )
    .unwrap();
    drop(release);
    tokio::time::timeout(Duration::from_secs(10), reload)
        .await
        .expect("the reload stalled")
        .unwrap();
    assert_eq!(parsed(), 1, "the scan parsed the changed file");
    assert!(
        env.rt.store().get("acme-a").is_some(),
        "the changed file lost its credential"
    );
    assert!(env.rt.store().get("slow-a").is_some());
    assert!(
        env.rt.store().get("1-new.json").is_none(),
        "published before it was parsed"
    );
    cpa_server::plugins::reload_plugin_auth_files(&env.rt).await;
    assert_eq!(env.rt.store().get("1-new.json").unwrap().provider, "claude");
    assert_eq!(parsed(), 2, "the next reload parses the new bytes");
    assert!(env.rt.store().get("acme-a").is_some());
    env.finish().await;
}

/// Asserts that, with nothing changing, the plugin auth epoch and `label`'s parse count
/// stay put over a second of watcher ticks: a reload and its config sync must not
/// retire the cached answers and reload again forever.
async fn settles(env: &Env, label: &str) {
    let now = || {
        (
            cpa_core::config::credentials::plugin_auth_epoch(),
            calls(&env.records, label, "auth.parse").len(),
        )
    };
    tokio::time::sleep(Duration::from_millis(300)).await;
    let before = now();
    tokio::time::sleep(Duration::from_secs(1)).await;
    assert_eq!(now(), before, "the auth reloads never settle (epoch, parses)");
}

/// Unloading an auth provider changes what its file loads to with no file changing:
/// the watcher reloads the auth folder on its own. Here P parsed `p.json` into two
/// credentials and Q, another auth provider, keeps plugin auth parsing on, so a reload
/// that only read cached answers would keep P's credentials. This is what the
/// management delete of a plugin does before it publishes (its file goes too, so a
/// later config sync does not load it again).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn unloading_an_auth_provider_reloads_its_files_without_a_file_change() {
    let Some(env) = env(
        "plugin-owned-auth-parse-unload",
        &[("pp", "auth_provider"), ("qq", "auth_provider")],
        "",
        &[("p.json", r#"{"type":"pp"}"#)],
        |records| {
            let auth = |id: &str| json!({"Provider":"pp","ID":id,"Metadata":{"type":"pp"}});
            canned(
                records,
                "pp",
                "auth.parse",
                json!({"Handled":true,"Auths":[auth("p1"), auth("p2")]}),
            );
        },
    )
    .await
    else {
        return;
    };
    let path = env.dir.join("config.yaml");
    std::fs::write(&path, &env.yaml).unwrap();
    let management = cpa_server::management::Management::new(env.rt.clone(), path);
    let watcher = cpa_server::watching::start(&management);
    let store = env.rt.store().clone();
    eventually("P's credentials never loaded", || {
        store.get("p1").is_some() && store.get("p2").is_some()
    })
    .await;
    // Nothing changes from here: the reloads and syncs settle.
    settles(&env, "pp").await;
    std::fs::remove_file(env.plugins.join("pp.so")).unwrap();
    assert!(env.rt.plugins().unload_plugin("pp", None).await);
    eventually("P's credentials outlived P", || {
        store.get("p1").is_none() && store.get("p2").is_none()
    })
    .await;
    // No plugin owns the type now: the built-in synthesizer loads the file.
    eventually("the file never loaded", || store.get("p.json").is_some()).await;
    settles(&env, "qq").await;
    assert!(store.get("p1").is_none() && store.get("p.json").is_some());
    watcher.abort();
    drop(management);
    env.finish().await;
}

/// Recorder `auth.parse` envelope answering credential `id` and saving `save` to the
/// file it parses (`host.auth.save` from inside the parse).
fn parse_saving(id: &str, save: Value) -> String {
    json!({
        "ok": true,
        "result": {"Handled": true, "Auths": [{"Provider":"acme","ID":id,"Metadata":{"type":"acme"}}]},
        "host_calls": [{"method": "host.auth.save", "request": {"name": "acme.json", "json": save}}]
    })
    .to_string()
}

/// Starts the config watcher over `env` (its config file written first).
fn watch(env: &Env) -> (Arc<cpa_server::management::Management>, tokio::task::JoinHandle<()>) {
    let path = env.dir.join("config.yaml");
    std::fs::write(&path, &env.yaml).unwrap();
    let management = cpa_server::management::Management::new(env.rt.clone(), path);
    let watcher = cpa_server::watching::start(&management);
    (management, watcher)
}

/// A plugin that saves the file it parses: the parse's answer is published, and the
/// saved bytes are parsed next, as Go parses the file again on the write's event. Here
/// parse(B0) answers `acme-old` and saves B1, and parse(B1) answers `acme-new` and saves
/// B1 again (unchanged), so the file ends up applied with parse(B1)'s credential.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_plugin_that_saves_its_file_while_parsing_ends_with_the_parse_of_the_saved_bytes() {
    let Some(env) = env(
        "plugin-owned-auth-parse-own-save",
        &[("acme", "auth_provider")],
        "",
        &[("acme.json", r#"{"type":"acme"}"#)],
        |records| {
            let dir = records.join("respond").join("acme");
            std::fs::create_dir_all(&dir).unwrap();
            let saved = json!({"type": "acme", "v": 1});
            std::fs::write(
                dir.join("auth.parse.once.json"),
                parse_saving("acme-old", saved.clone()),
            )
            .unwrap();
            std::fs::write(dir.join("auth.parse.json"), parse_saving("acme-new", saved)).unwrap();
        },
    )
    .await
    else {
        return;
    };
    let (management, watcher) = watch(&env);
    let store = env.rt.store().clone();
    eventually("parse(B1) was never published", || {
        store.get("acme-new").is_some() && store.get("acme-old").is_none()
    })
    .await;
    settles(&env, "acme").await;
    assert!(store.get("acme-new").is_some() && store.get("acme-old").is_none());
    watcher.abort();
    drop(management);
    env.finish().await;
}

/// A plugin that saves new bytes to the file it parses on every parse keeps being
/// asked again, as Go parses the file again on every write event, and the published
/// credential follows its recent parses.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_plugin_rewriting_its_file_on_every_parse_publishes_its_recent_parses() {
    let Some(env) = env(
        "plugin-owned-auth-parse-rewrite",
        &[("acme", "auth_provider")],
        "",
        &[("acme.json", r#"{"type":"acme"}"#)],
        |records| {
            let dir = records.join("respond").join("acme");
            std::fs::create_dir_all(&dir).unwrap();
            let envelope = parse_saving("acme-$seq", json!({"type": "acme", "n": "$seq"}));
            std::fs::write(dir.join("auth.parse.json"), envelope).unwrap();
        },
    )
    .await
    else {
        return;
    };
    let (management, watcher) = watch(&env);
    let published = || -> Option<u64> {
        env.rt
            .store()
            .snapshot()
            .iter()
            .filter_map(|c| c.id.strip_prefix("acme-")?.parse().ok())
            .max()
    };
    eventually("no parse was published", || published().is_some()).await;
    let first = published().unwrap();
    eventually("the published credential stopped following the parses", || {
        published().is_some_and(|n| n > first + 1)
    })
    .await;
    watcher.abort();
    drop(management);
    env.finish().await;
}

/// The auth identifier a plugin reports takes part in what its parses mean: one that
/// failed (no identifier, so the plugin owns no file type) and recovers on a later sync,
/// with the same plugin and config, makes the watcher parse the files again.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_auth_identifier_recovering_from_an_error_reparses_the_auth_files() {
    let identifier = |records: &Path, envelope: Value| {
        std::fs::write(records.join("respond/acme/auth.identifier.json"), envelope.to_string()).unwrap();
    };
    let Some(env) = env(
        "plugin-owned-auth-parse-identifier",
        &[("acme", "auth_provider")],
        "",
        &[("acme.json", r#"{"type":"acme"}"#)],
        |records| {
            canned(
                records,
                "acme",
                "auth.parse",
                json!({"Handled":true,"Auths":[{"Provider":"acme","ID":"acme-a","Metadata":{"type":"acme"}}]}),
            );
            identifier(
                records,
                json!({"ok": false, "error": {"code": "down", "message": "down"}}),
            );
        },
    )
    .await
    else {
        return;
    };
    let (management, watcher) = watch(&env);
    let store = env.rt.store().clone();
    eventually("the file never loaded", || store.get("acme.json").is_some()).await;
    settles(&env, "acme").await;
    assert!(store.get("acme-a").is_none(), "parsed without an identifier");
    identifier(&env.records, json!({"ok": true, "result": {"identifier": "acme"}}));
    cpa_server::plugins::sync(&env.rt, env.rt.config()).await;
    eventually("the recovered identifier's files were never parsed", || {
        store.get("acme-a").is_some()
    })
    .await;
    watcher.abort();
    drop(management);
    env.finish().await;
}

/// Every parse request carries the host config summary: a config change that only
/// touches it (here `requests.proxy-url`) makes the watcher ask the plugins again.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_host_config_change_reparses_the_auth_files() {
    let Some(env) = env(
        "plugin-owned-auth-parse-host-config",
        &[("acme", "auth_provider")],
        "",
        &[("acme.json", r#"{"type":"acme"}"#)],
        |records| {
            canned(
                records,
                "acme",
                "auth.parse",
                json!({"Handled":true,"Auths":[{"Provider":"acme","ID":"acme-a","Metadata":{"type":"acme"}}]}),
            );
        },
    )
    .await
    else {
        return;
    };
    let (management, watcher) = watch(&env);
    eventually("the file was never parsed", || env.rt.store().get("acme-a").is_some()).await;
    settles(&env, "acme").await;
    let proxy = "http://127.0.0.1:9";
    std::fs::write(
        env.dir.join("config.yaml"),
        format!("requests:\n  proxy-url: {proxy}\n{}", env.yaml),
    )
    .unwrap();
    eventually("the plugin was never asked with the new host config", || {
        calls(&env.records, "acme", "auth.parse")
            .iter()
            .any(|req| req["Host"]["ProxyURL"] == proxy)
    })
    .await;
    watcher.abort();
    drop(management);
    env.finish().await;
}

/// The binary runs the startup reload off the async workers: with one worker thread, a
/// parse can still make a host callback that needs the runtime (`host.http.do` to the
/// proxy this runtime serves).
#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn the_startup_reload_lets_a_parse_use_the_runtime_with_one_worker() {
    let Some(env) = env(
        "plugin-owned-auth-parse-one-worker",
        &[("acme", "auth_provider")],
        "",
        &[("acme.json", r#"{"type":"acme"}"#)],
        |records| std::fs::create_dir_all(records.join("respond/acme")).unwrap(),
    )
    .await
    else {
        return;
    };
    let envelope = json!({
        "ok": true,
        "result": {"Handled": true, "Auths": [{"Provider":"acme","ID":"acme-a","Metadata":{"type":"acme"}}]},
        "host_calls": [{"method": "host.http.do", "request": {"url": format!("http://{}/v1/models", env.proxy)}}]
    });
    std::fs::write(env.records.join("respond/acme/auth.parse.json"), envelope.to_string()).unwrap();
    tokio::time::timeout(
        Duration::from_secs(10),
        cpa_server::plugins::reload_plugin_auth_files(&env.rt),
    )
    .await
    .expect("the reload stalled");
    assert!(env.rt.store().get("acme-a").is_some());
    let response = &calls(&env.records, "acme", "host_callback")[0];
    assert_eq!(response["ok"], true, "{response}");
    assert!(response["result"]["StatusCode"].as_u64().is_some(), "{response}");
    env.finish().await;
}

/// The startup reload publishes its scan the way the watcher does (Go c4c1d43e): a token
/// refresh that commits while a slow plugin parse holds the reload keeps its token.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_refresh_committed_during_the_startup_reload_survives_it() {
    let Some(env) = env(
        "plugin-owned-auth-parse-refresh",
        &[("acme", "auth_provider")],
        "",
        &[
            // Read before the plugin's file, which sorts after it.
            ("a-claude.json", r#"{"type":"claude","access_token":"old"}"#),
            ("acme.json", r#"{"type":"acme","wrapped":"material"}"#),
        ],
        |records| {
            canned(
                records,
                "acme",
                "auth.parse",
                json!({"Handled":true,"Auths":[{"Provider":"acme","ID":"acme-a","Metadata":{"type":"acme"}}]}),
            );
            std::fs::write(records.join("respond/acme/auth.parse.hold"), "").unwrap();
        },
    )
    .await
    else {
        return;
    };
    let release = Release(env.records.join("respond/acme/auth.parse.hold"));
    let reload = tokio::spawn({
        let rt = env.rt.clone();
        async move { cpa_server::plugins::reload_plugin_auth_files(&rt).await }
    });
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while calls(&env.records, "acme", "auth.parse").is_empty() {
        assert!(std::time::Instant::now() < deadline, "the parse never started");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    // A refresh commits while the parse holds the reload.
    let store = env.rt.store();
    let current = store.get("a-claude.json").unwrap();
    let mut patch = cpa_core::credential::MetadataPatch::default();
    patch.set.insert("access_token".into(), "refreshed".into());
    store.apply_patch("a-claude.json", current.revision, &patch).unwrap();
    drop(release);
    reload.await.unwrap();
    assert!(
        store.get("acme-a").is_some(),
        "the reload published the plugin credential"
    );
    assert_eq!(
        store.get("a-claude.json").unwrap().metadata["access_token"],
        "refreshed"
    );
    env.finish().await;
}

/// The auth file `name` as JSON.
fn auth_file(env: &Env, name: &str) -> Value {
    serde_json::from_slice(&std::fs::read(env.rt.config().auth_dir.join(name)).unwrap()).unwrap()
}

/// Go `executorAdapter.Refresh` through `tryRefreshAfterUnauthorized`: a 401 from a
/// plugin executor refreshes its credential through the plugin's auth provider, which
/// sees the stored document, and the retry carries the refreshed token. The refreshed
/// metadata overlays the stored document, which keeps the keys the plugin left out.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_401_from_a_plugin_executor_refreshes_through_its_auth_provider() {
    let Some(env) = env(
        "plugin-owned-refresh-401",
        &[("catalog", "model_provider"), ("acme", "auth_provider,executor")],
        "",
        &[(
            "acme.json",
            r#"{"type":"acme","access_token":"old","refresh_token":"r1","keep":"x"}"#,
        )],
        |records| {
            canned(
                records,
                "catalog",
                "model.static",
                json!({"Provider":"acme","Models":[{"ID":"acme-1"}]}),
            );
            canned(
                records,
                "acme",
                "auth.refresh",
                json!({"Auth":{"Metadata":{"type":"acme","access_token":"new","refresh_token":"r2"}}}),
            );
            std::fs::write(
                records.join("respond/acme/executor.execute.once.json"),
                json!({"ok":false,"error":{"code":"unauthorized","message":"token expired","http_status":401}})
                    .to_string(),
            )
            .unwrap();
        },
    )
    .await
    else {
        return;
    };
    let (status, body) = env.chat("acme-1").await;
    assert_eq!(status, 200, "{body}");
    let refreshes = calls(&env.records, "acme", "auth.refresh");
    assert_eq!(refreshes.len(), 1, "{refreshes:?}");
    assert_eq!(refreshes[0]["AuthID"], "acme.json");
    assert_eq!(refreshes[0]["AuthProvider"], "acme");
    assert_eq!(refreshes[0]["Metadata"]["refresh_token"], "r1");
    let executed = calls(&env.records, "acme", "executor.execute");
    assert_eq!(executed.len(), 2, "one retry after the refresh");
    assert_eq!(executed[0]["AuthMetadata"]["access_token"], "old");
    assert_eq!(executed[1]["AuthMetadata"]["access_token"], "new");
    assert_eq!(
        auth_file(&env, "acme.json"),
        json!({"type":"acme","access_token":"new","refresh_token":"r2","keep":"x","disabled":false})
    );
    env.finish().await;
}

/// Go `pluginRefreshCompatExecutor`: a credential with no plugin executor refreshes
/// through the auth provider of its provider. When only its `compat_name` matched an
/// auth provider, no plugin answers for its provider, and with a refresh token the
/// refresh fails with Go's error instead of keeping the credential.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_compatible_credential_refreshes_through_its_providers_auth_provider() {
    let Some(env) = env(
        "plugin-owned-refresh-compat",
        &[("acme", "auth_provider"), ("pool", "auth_provider")],
        "",
        &[
            (
                "acme.json",
                r#"{"type":"acme","access_token":"old","refresh_token":"r1"}"#,
            ),
            ("pool.json", r#"{"type":"pool","wrapped":"material"}"#),
        ],
        |records| {
            canned(
                records,
                "acme",
                "auth.refresh",
                json!({"Auth":{"Metadata":{"type":"acme","access_token":"new"}}}),
            );
            canned(
                records,
                "pool",
                "auth.parse",
                json!({"Handled":true,"Auths":[{"Provider":"zeta","ID":"zeta-1",
                    "Attributes":{"compat_name":"pool"},
                    "Metadata":{"type":"zeta","access_token":"z","refresh_token":"rz"}}]}),
            );
        },
    )
    .await
    else {
        return;
    };
    cpa_server::plugins::reload_plugin_auth_files(&env.rt).await;
    // Storage that does not decode as an object fails the refresh (Go cannot persist it)
    // and leaves the stored document alone.
    std::fs::write(
        env.records.join("respond/acme/auth.refresh.once.json"),
        json!({"ok":true,"result":{"Auth":{"StorageJSON":"W10=","Metadata":{"type":"acme","access_token":"bad"}}}})
            .to_string(),
    )
    .unwrap();
    let error = env.rt.refresh_credential("acme.json").await.unwrap_err();
    assert!(
        String::from_utf8_lossy(&error.body).contains("persist refreshed auth"),
        "{error:?}"
    );
    assert_eq!(
        auth_file(&env, "acme.json"),
        json!({"type":"acme","access_token":"old","refresh_token":"r1"})
    );
    let refreshed = env.rt.refresh_credential("acme.json").await.unwrap();
    assert_eq!(refreshed.metadata["access_token"], "new");
    assert_eq!(
        refreshed.metadata["refresh_token"], "r1",
        "kept from the stored document"
    );
    assert_eq!(calls(&env.records, "acme", "auth.refresh").len(), 2);
    let before = env.rt.store().get("zeta-1").expect("the parsed credential");
    let error = env.rt.refresh_credential("zeta-1").await.unwrap_err();
    assert!(
        String::from_utf8_lossy(&error.body)
            .contains("plugin auth provider refresh is unavailable for provider openai-compatible-pool"),
        "{error:?}"
    );
    assert!(calls(&env.records, "pool", "auth.refresh").is_empty());
    assert_eq!(env.rt.store().get("zeta-1").unwrap().revision, before.revision);
    env.finish().await;
}

/// Go `shouldRefresh` for a plugin-refreshed credential: only `refresh_interval`
/// schedules a refresh. A credential never refreshed is due at once; after a refresh the
/// next one waits the interval from it (Go `LastRefreshedAt`), whatever
/// `NextRefreshAfter` the plugin answered (Go clears it when it commits the refresh).
/// A recent `last_refresh` holds the refresh off, and no interval never schedules one.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_plugin_refresh_interval_schedules_background_refreshes() {
    let recent = (chrono::Utc::now() - chrono::Duration::minutes(10)).to_rfc3339();
    let held = format!(r#"{{"type":"acme","access_token":"h","refresh_interval":"1h","last_refresh":"{recent}"}}"#);
    let Some(env) = env(
        "plugin-owned-refresh-interval",
        &[("acme", "auth_provider")],
        "",
        &[
            (
                "due.json",
                r#"{"type":"acme","access_token":"old","refresh_interval":"2s"}"#,
            ),
            ("held.json", &held),
            (
                "never.json",
                r#"{"type":"acme","access_token":"n","refresh_token":"rn"}"#,
            ),
        ],
        |records| {
            canned(
                records,
                "acme",
                "auth.refresh",
                json!({"Auth":{"Metadata":{"type":"acme","access_token":"new"}},
                    "NextRefreshAfter":"2099-01-01T00:00:00Z"}),
            );
        },
    )
    .await
    else {
        return;
    };
    // The binary's startup load, as plugins parse auth files.
    cpa_server::plugins::reload_plugin_auth_files(&env.rt).await;
    env.rt.start_auto_refresh();
    let refreshed = || {
        calls(&env.records, "acme", "auth.refresh")
            .iter()
            .map(|r| r["AuthID"].as_str().unwrap().to_owned())
            .collect::<Vec<_>>()
    };
    eventually("the first refresh", || !refreshed().is_empty()).await;
    let first = std::time::Instant::now();
    eventually("the second refresh", || refreshed().len() >= 2).await;
    assert!(
        first.elapsed() >= Duration::from_millis(1500),
        "the second refresh waits the interval: {:?}",
        first.elapsed()
    );
    assert_eq!(refreshed(), ["due.json", "due.json"]);
    assert_eq!(env.rt.store().get("due.json").unwrap().metadata["access_token"], "new");
    env.stop_refreshes().await;
    env.finish().await;
}

/// Go's `NextRefreshAfter` and `LastRefreshedAt` for plugin-refreshed credentials,
/// each due by its `refresh_interval` at once (as the control `c.json` is): a parsed
/// auth waits for the `NextRefreshAfter` its plugin parsed, a failed management refresh
/// holds the next one off for 5 min, and a refresh whose result an unrelated edit made
/// a no-op still counts as the last refresh.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn plugin_refresh_deadlines_follow_gos_lifecycle_fields() {
    let Some(env) = env(
        "plugin-owned-refresh-deadlines",
        &[
            ("acme", "auth_provider"),
            ("pp", "auth_provider"),
            ("qq", "auth_provider"),
            ("rr", "auth_provider"),
        ],
        "",
        &[
            (
                "c.json",
                r#"{"type":"acme","access_token":"c","refresh_interval":"1h"}"#,
            ),
            (
                "f.json",
                r#"{"type":"acme","access_token":"f","refresh_interval":"1h"}"#,
            ),
            (
                "s.json",
                r#"{"type":"acme","access_token":"s","refresh_interval":"1h","disabled":false}"#,
            ),
            ("p.json", r#"{"type":"pp","wrapped":"material"}"#),
            // Loaded before the plugins start, as the binary loads it, under the ID the
            // plugin then parses it as.
            ("q.json", r#"{"type":"qq","wrapped":"material"}"#),
            // Parsed as the very credential the startup load built from it, but for its
            // deadline.
            ("r.json", r#"{"type":"rr","access_token":"r","refresh_interval":"1h"}"#),
        ],
        |records| {
            // An answer without auth fields keeps the credential as it was.
            canned(records, "acme", "auth.refresh", json!({"Auth":{}}));
            canned(
                records,
                "pp",
                "auth.parse",
                json!({"Handled":true,"Auths":[{"Provider":"pp","ID":"p-1",
                    "Metadata":{"type":"pp","access_token":"p","refresh_interval":"1h"},
                    "NextRefreshAfter":"2099-01-01T00:00:00Z"}]}),
            );
            canned(
                records,
                "qq",
                "auth.parse",
                json!({"Handled":true,"Auths":[{"Provider":"qq","ID":"q.json",
                    "Metadata":{"type":"qq","access_token":"q","refresh_interval":"1h"},
                    "NextRefreshAfter":"2099-01-01T00:00:00Z"}]}),
            );
            canned(
                records,
                "rr",
                "auth.parse",
                json!({"Handled":true,"Auths":[{"Provider":"rr","ID":"r.json","Label":"rr",
                    "Metadata":{"type":"rr","access_token":"r","refresh_interval":"1h"},
                    "NextRefreshAfter":"2099-01-01T00:00:00Z"}]}),
            );
        },
    )
    .await
    else {
        return;
    };
    cpa_server::plugins::reload_plugin_auth_files(&env.rt).await;
    assert!(env.rt.store().get("p-1").is_some());
    assert_eq!(env.rt.store().get("q.json").unwrap().metadata["access_token"], "q");
    let refreshed = |id: &str| {
        calls(&env.records, "acme", "auth.refresh")
            .iter()
            .filter(|r| r["AuthID"] == id)
            .count()
    };
    std::fs::write(
        env.records.join("respond/acme/auth.refresh.once.json"),
        json!({"ok":false,"error":{"code":"upstream","message":"refresh failed","http_status":502}}).to_string(),
    )
    .unwrap();
    env.rt.refresh_credential("f.json").await.unwrap_err();
    // An edit of other fields lands while the plugin refreshes `s.json`.
    let release = Release(env.records.join("respond/acme/auth.refresh.hold"));
    std::fs::write(&release.0, "").unwrap();
    let pending = tokio::spawn({
        let rt = env.rt.clone();
        async move { rt.refresh_credential("s.json").await }
    });
    eventually("the refresh of s.json", || refreshed("s.json") == 1).await;
    let store = env.rt.store();
    let mut edit = cpa_core::credential::MetadataPatch::default();
    edit.set.insert("note".into(), "edited".into());
    store
        .apply_patch("s.json", store.get("s.json").unwrap().revision, &edit)
        .unwrap();
    drop(release);
    pending.await.unwrap().unwrap();
    assert_eq!(store.get("s.json").unwrap().metadata["note"], "edited");

    env.rt.start_auto_refresh();
    eventually("the control refresh", || refreshed("c.json") >= 1).await;
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert_eq!(refreshed("f.json"), 1, "backing off after the failed refresh");
    assert_eq!(refreshed("s.json"), 1, "the no-op refresh counts as the last one");
    for label in ["pp", "qq", "rr"] {
        assert!(
            calls(&env.records, label, "auth.refresh").is_empty(),
            "{label} waiting for the parsed NextRefreshAfter"
        );
    }
    env.rt.stop_auto_refresh();
    env.finish().await;
}

/// Go `refreshAuth` after a failed refresh while the access token is still valid: the
/// next one waits 5 min or until the token expires, whichever comes first. A
/// background refresh that fails seconds before expiry retries at the expiry, not after
/// the shared 5 min backoff of built-in refreshes.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_failed_background_plugin_refresh_retries_when_the_token_expires() {
    let Some(env) = env(
        "plugin-owned-refresh-expiry",
        &[("acme", "auth_provider")],
        "",
        &[(
            "e.json",
            r#"{"type":"acme","access_token":"e","refresh_interval":"1h"}"#,
        )],
        |records| {
            canned(
                records,
                "acme",
                "auth.refresh",
                json!({"Auth":{"Metadata":{"type":"acme","access_token":"e2","refresh_interval":"1h",
                    "expired":"2099-01-01T00:00:00Z"}}}),
            );
            std::fs::write(
                records.join("respond/acme/auth.refresh.once.json"),
                json!({"ok":false,"error":{"code":"upstream","message":"refresh failed","http_status":502}})
                    .to_string(),
            )
            .unwrap();
        },
    )
    .await
    else {
        return;
    };
    // The token expires 3 s from now; the first refresh fails at once, before that.
    let expiry = chrono::Utc::now() + chrono::Duration::seconds(3);
    let store = env.rt.store();
    let mut patch = cpa_core::credential::MetadataPatch::default();
    patch.set.insert("expired".into(), expiry.to_rfc3339().into());
    store
        .apply_patch("e.json", store.get("e.json").unwrap().revision, &patch)
        .unwrap();
    let refreshes = || calls(&env.records, "acme", "auth.refresh").len();
    env.rt.start_auto_refresh();
    eventually("the failing refresh", || refreshes() >= 1).await;
    assert!(chrono::Utc::now() < expiry, "the first refresh ran before the expiry");
    eventually("the retry at the expiry", || refreshes() >= 2).await;
    eventually("the retry's commit", || {
        env.rt.store().get("e.json").unwrap().metadata["access_token"] == "e2"
    })
    .await;
    env.rt.stop_auto_refresh();
    env.finish().await;
}

/// A plugin refresh whose result cannot be saved backs off like a failed refresh,
/// instead of refreshing again at once on every pass of the refresh loop.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_plugin_refresh_that_cannot_be_saved_backs_off() {
    use std::os::unix::fs::PermissionsExt;
    let Some(env) = env(
        "plugin-owned-refresh-unsaved",
        &[("acme", "auth_provider")],
        "",
        &[(
            "u.json",
            r#"{"type":"acme","access_token":"u","refresh_interval":"1h"}"#,
        )],
        |records| {
            canned(
                records,
                "acme",
                "auth.refresh",
                json!({"Auth":{"Metadata":{"type":"acme","access_token":"u2","refresh_interval":"1h"}}}),
            );
        },
    )
    .await
    else {
        return;
    };
    let auth_dir = env.rt.config().auth_dir.clone();
    let mode = |m: u32| std::fs::set_permissions(&auth_dir, std::fs::Permissions::from_mode(m)).unwrap();
    // A read-only auth dir: the atomic write of the refreshed file fails.
    mode(0o555);
    if std::fs::write(auth_dir.join("probe"), "").is_ok() {
        // Running as root: permissions do not stop the write.
        mode(0o755);
        return;
    }
    let refreshes = || calls(&env.records, "acme", "auth.refresh").len();
    // The binary's startup load, as plugins parse auth files.
    cpa_server::plugins::reload_plugin_auth_files(&env.rt).await;
    env.rt.start_auto_refresh();
    eventually("the refresh", || refreshes() >= 1).await;
    tokio::time::sleep(Duration::from_millis(3000)).await;
    env.rt.stop_auto_refresh();
    mode(0o755);
    assert_eq!(refreshes(), 1, "backing off after the unsaved refresh");
    assert_eq!(auth_file(&env, "u.json")["access_token"], "u");
    env.finish().await;
}

/// A parsed `NextRefreshAfter` seeds the registration it came with only. A credential
/// registered afresh is due by its interval at once, as Go registers it: after its
/// file is deleted and created again as an ordinary credential the parser no longer
/// handles (whether or not the refresh loop saw the parsed one). A credential disabled
/// and enabled again takes the deadline it is registered with: the parsed one again.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_credential_registered_afresh_does_not_inherit_a_parsed_refresh_deadline() {
    let parsed = |id: &str| {
        json!({"Handled":true,"Auths":[{"Provider":"pp","ID":id,
            "Metadata":{"type":"pp","access_token":"p","refresh_interval":"1h"},
            "NextRefreshAfter":"2099-01-01T00:00:00Z"}]})
    };
    let Some(env) = env(
        "plugin-owned-refresh-seed",
        &[("pp", "auth_provider")],
        "",
        &[],
        |records| {
            canned(
                records,
                "pp",
                "auth.refresh",
                json!({"Auth":{"Metadata":{"type":"pp","access_token":"p2","refresh_interval":"1h"}}}),
            );
        },
    )
    .await
    else {
        return;
    };
    let auth_dir = env.rt.config().auth_dir.clone();
    let refreshes = |id: &str| {
        calls(&env.records, "pp", "auth.refresh")
            .iter()
            .filter(|r| r["AuthID"] == id)
            .count()
    };
    let write = |name: &str, body: &str| std::fs::write(auth_dir.join(name), body).unwrap();
    // A refresh is done when its tokens are committed, in the store and on disk: one
    // committing after its section ends would write its file back into the next.
    let committed = |id: &str| {
        let on_disk = std::fs::read(auth_dir.join(id))
            .ok()
            .and_then(|b| serde_json::from_slice::<Value>(&b).ok());
        env.rt
            .store()
            .get(id)
            .is_some_and(|c| c.metadata["access_token"] == "p2")
            && on_disk.is_some_and(|d| d["access_token"] == "p2")
    };
    // What a missing refresh looks like, for the failure message.
    let state = || {
        let store: Vec<String> = env
            .rt
            .store()
            .snapshot()
            .iter()
            .map(|c| format!("{} {:?} {}", c.id, c.source, c.metadata["access_token"]))
            .collect();
        let parsed: Vec<Value> = calls(&env.records, "pp", "auth.parse")
            .iter()
            .map(|r| r["Path"].clone())
            .collect();
        let files: Vec<_> = std::fs::read_dir(&auth_dir)
            .unwrap()
            .flatten()
            .map(|e| e.file_name())
            .collect();
        format!(
            "store {store:?}, parsed {parsed:?}, files {files:?}, refreshed {:?}",
            calls(&env.records, "pp", "auth.refresh")
                .iter()
                .map(|r| r["AuthID"].clone())
                .collect::<Vec<_>>()
        )
    };
    let wrapped = r#"{"type":"pp","wrapped":"material"}"#;
    let ordinary = r#"{"type":"pp","access_token":"q","refresh_interval":"1h"}"#;
    // The refresh loop coalesces store changes for 1 s.
    let settle = || tokio::time::sleep(Duration::from_millis(2000));

    // Seen by the refresh loop, deleted, then created again.
    canned(&env.records, "pp", "auth.parse", parsed("a.json"));
    write("a.json", wrapped);
    cpa_server::plugins::reload_plugin_auth_files(&env.rt).await;
    env.rt.start_auto_refresh();
    settle().await;
    assert_eq!(refreshes("a.json"), 0, "waiting for the parsed NextRefreshAfter");
    std::fs::remove_file(auth_dir.join("a.json")).unwrap();
    cpa_server::plugins::reload_plugin_auth_files(&env.rt).await;
    assert!(env.rt.store().get("a.json").is_none());
    settle().await;
    canned(&env.records, "pp", "auth.parse", json!({"Handled":false}));
    write("a.json", ordinary);
    cpa_server::plugins::reload_plugin_auth_files(&env.rt).await;
    assert_eq!(env.rt.store().get("a.json").unwrap().metadata["access_token"], "q");
    eventually("a.json's committed refresh", || committed("a.json")).await;
    env.stop_refreshes().await;
    std::fs::remove_file(auth_dir.join("a.json")).unwrap();

    // Parsed, deleted and created again before the refresh loop runs.
    canned(&env.records, "pp", "auth.parse", parsed("b.json"));
    write("b.json", wrapped);
    cpa_server::plugins::reload_plugin_auth_files(&env.rt).await;
    std::fs::remove_file(auth_dir.join("b.json")).unwrap();
    cpa_server::plugins::reload_plugin_auth_files(&env.rt).await;
    canned(&env.records, "pp", "auth.parse", json!({"Handled":false}));
    write("b.json", ordinary);
    cpa_server::plugins::reload_plugin_auth_files(&env.rt).await;
    assert!(env.rt.store().get("a.json").is_none(), "{}", state());
    env.rt.start_auto_refresh();
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while !committed("b.json") {
        assert!(std::time::Instant::now() < deadline, "b.json's refresh: {}", state());
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    env.stop_refreshes().await;
    std::fs::remove_file(auth_dir.join("b.json")).unwrap();

    // Disabled and enabled again.
    canned(&env.records, "pp", "auth.parse", parsed("c.json"));
    write("c.json", wrapped);
    cpa_server::plugins::reload_plugin_auth_files(&env.rt).await;
    env.rt.start_auto_refresh();
    settle().await;
    assert_eq!(refreshes("c.json"), 0, "waiting for the parsed NextRefreshAfter");
    let store = env.rt.store();
    for disabled in [true, false] {
        let mut patch = cpa_core::credential::MetadataPatch::default();
        patch.set.insert("disabled".into(), disabled.into());
        store
            .apply_patch("c.json", store.get("c.json").unwrap().revision, &patch)
            .unwrap();
        settle().await;
    }
    assert_eq!(refreshes("c.json"), 0, "still waiting for the parsed NextRefreshAfter");
    env.stop_refreshes().await;
    env.finish().await;
}

/// A refresh env for plugin `pp`, whose refresh answers with new tokens, and how many
/// times it refreshed auth `id`.
async fn pp_refresh_env(name: &str) -> Option<(Env, impl Fn(&str) -> usize)> {
    let env = env(name, &[("pp", "auth_provider")], "", &[], |records| {
        canned(
            records,
            "pp",
            "auth.refresh",
            json!({"Auth":{"Metadata":{"type":"pp","access_token":"p2","refresh_interval":"1h"}}}),
        );
    })
    .await?;
    let records = env.records.clone();
    let refreshes = move |id: &str| {
        calls(&records, "pp", "auth.refresh")
            .iter()
            .filter(|r| r["AuthID"] == id)
            .count()
    };
    Some((env, refreshes))
}

/// A parsed `NextRefreshAfter` belongs to the registration it came with, whichever file
/// registers the auth next: `a.json` registers auth `b.json` disabled, so the refresh
/// loop never looks at its deadline, and is deleted; file `b.json` then registers
/// `b.json` as an ordinary credential the parser does not handle, which is due by its
/// interval at once.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_auth_another_file_registers_does_not_inherit_a_parsed_refresh_deadline() {
    let Some((env, refreshes)) = pp_refresh_env("plugin-owned-refresh-cross-file").await else {
        return;
    };
    let auth_dir = env.rt.config().auth_dir.clone();
    canned(
        &env.records,
        "pp",
        "auth.parse",
        json!({"Handled":true,"Auths":[{"Provider":"pp","ID":"b.json",
            "Metadata":{"type":"pp","access_token":"p","refresh_interval":"1h"},
            "NextRefreshAfter":"2099-01-01T00:00:00Z"}]}),
    );
    std::fs::write(
        auth_dir.join("a.json"),
        r#"{"type":"pp","wrapped":"material","disabled":true}"#,
    )
    .unwrap();
    cpa_server::plugins::reload_plugin_auth_files(&env.rt).await;
    assert!(env.rt.store().get("b.json").unwrap().disabled);
    std::fs::remove_file(auth_dir.join("a.json")).unwrap();
    cpa_server::plugins::reload_plugin_auth_files(&env.rt).await;
    assert!(env.rt.store().get("b.json").is_none());
    canned(&env.records, "pp", "auth.parse", json!({"Handled":false}));
    std::fs::write(
        auth_dir.join("b.json"),
        r#"{"type":"pp","access_token":"q","refresh_interval":"1h"}"#,
    )
    .unwrap();
    cpa_server::plugins::reload_plugin_auth_files(&env.rt).await;
    assert_eq!(env.rt.store().get("b.json").unwrap().metadata["access_token"], "q");
    env.rt.start_auto_refresh();
    eventually("b.json's refresh", || refreshes("b.json") >= 1).await;
    env.finish().await;
}

/// Go keeps a credential's `LastRefreshedAt` and `NextRefreshAfter` when it is
/// registered again only while it and the one it replaces are both enabled. A reload
/// that edits it keeps them; one that disables it, and the one that enables it again,
/// take the registered ones: no last refresh, so it is due by its interval at once.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_disabling_reload_drops_a_credentials_refresh_schedule() {
    let Some((env, refreshes)) = pp_refresh_env("plugin-owned-refresh-disabled-reload").await else {
        return;
    };
    let auth_dir = env.rt.config().auth_dir.clone();
    canned(&env.records, "pp", "auth.parse", json!({"Handled":false}));
    let reload = |body: Value| {
        std::fs::write(auth_dir.join("d.json"), body.to_string()).unwrap();
        cpa_server::plugins::reload_plugin_auth_files(&env.rt)
    };
    let file = |extra: Value| {
        let mut body = json!({"type":"pp","access_token":"q","refresh_interval":"1h"});
        body.as_object_mut().unwrap().extend(extra.as_object().unwrap().clone());
        body
    };
    reload(file(json!({}))).await;
    env.rt.start_auto_refresh();
    eventually("the first refresh", || refreshes("d.json") == 1).await;
    let refreshed = |c: &cpa_core::credential::Credential| c.metadata["access_token"] == "p2";
    eventually("the committed refresh", || {
        refreshed(&env.rt.store().get("d.json").unwrap())
    })
    .await;
    reload(file(json!({"note":"edited"}))).await;
    assert_eq!(env.rt.store().get("d.json").unwrap().metadata["note"], "edited");
    // The refresh loop coalesces store changes for 1 s.
    tokio::time::sleep(Duration::from_millis(2000)).await;
    assert_eq!(refreshes("d.json"), 1, "an edit keeps the last refresh");
    reload(file(json!({"disabled":true}))).await;
    assert!(env.rt.store().get("d.json").unwrap().disabled);
    reload(file(json!({}))).await;
    eventually("the refresh after the enabling reload", || refreshes("d.json") == 2).await;
    env.finish().await;
}

/// Go starts refreshing once its first load, which asks plugins, has registered the
/// credentials (`Service.Run`: the parser, then `Load`, then `StartAutoRefresh`). Here
/// the refresh loop runs from the start, as the binary starts it, and leaves the startup
/// load's credentials alone until the plugin-aware load registers them: a credential due
/// by its interval that the plugin then dates to 2099 is never refreshed.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn startup_refreshes_wait_for_the_plugin_aware_load() {
    let Some(env) = env(
        "plugin-owned-refresh-startup-order",
        &[("pp", "auth_provider")],
        "",
        &[("s.json", r#"{"type":"pp","access_token":"s","refresh_interval":"1h"}"#)],
        |records| {
            canned(
                records,
                "pp",
                "auth.parse",
                json!({"Handled":true,"Auths":[{"Provider":"pp","ID":"s.json",
                    "Metadata":{"type":"pp","access_token":"p","refresh_interval":"1h"},
                    "NextRefreshAfter":"2099-01-01T00:00:00Z"}]}),
            );
            canned(
                records,
                "pp",
                "auth.refresh",
                json!({"Auth":{"Metadata":{"type":"pp","access_token":"p2","refresh_interval":"1h"}}}),
            );
        },
    )
    .await
    else {
        return;
    };
    let refreshes = || calls(&env.records, "pp", "auth.refresh").len();
    let hold = Release(env.records.join("respond/pp/auth.parse.hold"));
    std::fs::write(&hold.0, "").unwrap();
    env.rt.start_auto_refresh();
    let reload = tokio::spawn({
        let rt = env.rt.clone();
        async move { cpa_server::plugins::reload_plugin_auth_files(&rt).await }
    });
    eventually("the held parse", || !calls(&env.records, "pp", "auth.parse").is_empty()).await;
    // A caller that stops waiting leaves the load, and the startup window, running.
    reload.abort();
    // The refresh loop coalesces store changes for 1 s.
    tokio::time::sleep(Duration::from_millis(2500)).await;
    assert_eq!(refreshes(), 0, "refreshed before the plugin-aware load");
    drop(hold);
    eventually("the plugin-aware load", || {
        env.rt.store().get("s.json").unwrap().metadata["access_token"] == "p"
    })
    .await;
    tokio::time::sleep(Duration::from_millis(2000)).await;
    assert_eq!(refreshes(), 0, "waiting for the parsed NextRefreshAfter");
    env.finish().await;
}

/// Go fences a refresh by the registration it started under (its `RegistrationEpoch`):
/// a refresh held while its credential is removed and registered again under the same
/// ID, with the same tokens, changes nothing of the new registration. A failed one
/// leaves no 5 min backoff; a committed one is refused before it merges or saves its
/// tokens and `last_refresh`, and records no last refresh. Both new registrations are
/// due by their interval at once.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_refresh_from_an_earlier_registration_leaves_the_new_ones_schedule() {
    let Some((env, refreshes)) = pp_refresh_env("plugin-owned-refresh-registration").await else {
        return;
    };
    canned(&env.records, "pp", "auth.parse", json!({"Handled":false}));
    let auth_dir = env.rt.config().auth_dir.clone();
    let body = r#"{"type":"pp","access_token":"q","refresh_interval":"1h"}"#;
    for name in ["f.json", "c.json"] {
        std::fs::write(auth_dir.join(name), body).unwrap();
    }
    cpa_server::plugins::reload_plugin_auth_files(&env.rt).await;
    // `f.json`'s refresh fails, `c.json`'s answers new tokens and a last refresh; each is
    // held while its credential is removed and registered again.
    for (name, response) in [
        (
            "f.json",
            json!({"ok":false,"error":{"code":"upstream","message":"refresh failed","http_status":502}}),
        ),
        (
            "c.json",
            json!({"ok":true,"result":{"Auth":{"Metadata":{"type":"pp","access_token":"p2",
                "refresh_interval":"1h","last_refresh":chrono::Utc::now().to_rfc3339()}}}}),
        ),
    ] {
        std::fs::write(
            env.records.join("respond/pp/auth.refresh.once.json"),
            response.to_string(),
        )
        .unwrap();
        let hold = Release(env.records.join("respond/pp/auth.refresh.hold"));
        std::fs::write(&hold.0, "").unwrap();
        let since = std::time::Instant::now();
        let pending = tokio::spawn({
            let (rt, name) = (env.rt.clone(), name.to_owned());
            async move { rt.refresh_credential(&name).await }
        });
        eventually("the held refresh", || refreshes(name) == 1).await;
        std::fs::remove_file(auth_dir.join(name)).unwrap();
        cpa_server::plugins::reload_plugin_auth_files(&env.rt).await;
        assert!(env.rt.store().get(name).is_none());
        std::fs::write(auth_dir.join(name), body).unwrap();
        cpa_server::plugins::reload_plugin_auth_files(&env.rt).await;
        assert!(env.rt.store().get(name).is_some());
        release(hold, since, &pending);
        assert!(pending.await.unwrap().is_err(), "{name}: Go's stale registration epoch");
        let on_disk: Value = serde_json::from_slice(&std::fs::read(auth_dir.join(name)).unwrap()).unwrap();
        let in_memory = env.rt.store().get(name).unwrap();
        for document in [&on_disk, &Value::Object(in_memory.metadata.clone())] {
            assert_eq!(document["access_token"], "q", "{name}");
            assert!(document.get("last_refresh").is_none(), "{name}");
        }
    }
    env.rt.start_auto_refresh();
    eventually("both new registrations' refreshes", || {
        refreshes("f.json") == 2 && refreshes("c.json") == 2
    })
    .await;
    env.finish().await;
}

/// Go drops a refresh whose credential version moved since it started
/// (`CredentialVersion`), even when the credentials were put back since: a refresh
/// held while the file's token goes from `q` to `b` and back to `q` saves neither its
/// tokens nor its `last_refresh`, and records no last refresh, so the credential is
/// due by its interval at once.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_refresh_across_a_token_replacement_and_back_is_dropped() {
    let Some((env, refreshes)) = pp_refresh_env("plugin-owned-refresh-credential-version").await else {
        return;
    };
    canned(&env.records, "pp", "auth.parse", json!({"Handled":false}));
    let auth_dir = env.rt.config().auth_dir.clone();
    let write = |token: &str| {
        let body = json!({"type":"pp","access_token":token,"refresh_interval":"1h"});
        std::fs::write(auth_dir.join("x.json"), body.to_string()).unwrap();
    };
    write("q");
    cpa_server::plugins::reload_plugin_auth_files(&env.rt).await;
    std::fs::write(
        env.records.join("respond/pp/auth.refresh.once.json"),
        json!({"ok":true,"result":{"Auth":{"Metadata":{"type":"pp","access_token":"p2",
            "refresh_interval":"1h","last_refresh":chrono::Utc::now().to_rfc3339()}}}})
        .to_string(),
    )
    .unwrap();
    let hold = Release(env.records.join("respond/pp/auth.refresh.hold"));
    std::fs::write(&hold.0, "").unwrap();
    let since = std::time::Instant::now();
    let pending = tokio::spawn({
        let rt = env.rt.clone();
        async move { rt.refresh_credential("x.json").await }
    });
    eventually("the held refresh", || refreshes("x.json") == 1).await;
    for token in ["b", "q"] {
        write(token);
        cpa_server::plugins::reload_plugin_auth_files(&env.rt).await;
        assert_eq!(env.rt.store().get("x.json").unwrap().metadata["access_token"], token);
    }
    release(hold, since, &pending);
    pending.await.unwrap().unwrap();
    let on_disk: Value = serde_json::from_slice(&std::fs::read(auth_dir.join("x.json")).unwrap()).unwrap();
    let in_memory = env.rt.store().get("x.json").unwrap();
    for document in [&on_disk, &Value::Object(in_memory.metadata.clone())] {
        assert_eq!(document["access_token"], "q");
        assert!(document.get("last_refresh").is_none());
    }
    env.rt.start_auto_refresh();
    eventually("the refresh due at once", || refreshes("x.json") == 2).await;
    env.finish().await;
}

/// A failed refresh holds the next one off until the access token expires when that
/// is sooner than 5 min, reading the credential as it is when the refresh fails (Go
/// `refreshAuth` reads `m.auths[id]` under its lock): an edit while the refresh is held
/// that only brings the expiry closer to a few seconds away has the next refresh then,
/// not 5 min on.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_failed_refresh_backs_off_to_the_current_expiry() {
    let Some((env, refreshes)) = pp_refresh_env("plugin-owned-refresh-current-expiry").await else {
        return;
    };
    canned(&env.records, "pp", "auth.parse", json!({"Handled":false}));
    let auth_dir = env.rt.config().auth_dir.clone();
    let write = |expires_in: chrono::Duration| {
        let expired = (chrono::Utc::now() + expires_in).to_rfc3339();
        let body = json!({"type":"pp","access_token":"q","refresh_interval":"1h","expired":expired});
        std::fs::write(auth_dir.join("e.json"), body.to_string()).unwrap();
    };
    write(chrono::Duration::hours(1));
    cpa_server::plugins::reload_plugin_auth_files(&env.rt).await;
    std::fs::write(
        env.records.join("respond/pp/auth.refresh.once.json"),
        json!({"ok":false,"error":{"code":"upstream","message":"refresh failed","http_status":502}}).to_string(),
    )
    .unwrap();
    let hold = Release(env.records.join("respond/pp/auth.refresh.hold"));
    std::fs::write(&hold.0, "").unwrap();
    let since = std::time::Instant::now();
    let pending = tokio::spawn({
        let rt = env.rt.clone();
        async move { rt.refresh_credential("e.json").await }
    });
    eventually("the held refresh", || refreshes("e.json") == 1).await;
    let revision = env.rt.store().get("e.json").unwrap().revision;
    write(chrono::Duration::seconds(8));
    cpa_server::plugins::reload_plugin_auth_files(&env.rt).await;
    assert_ne!(env.rt.store().get("e.json").unwrap().revision, revision);
    release(hold, since, &pending);
    assert!(pending.await.unwrap().is_err());
    env.rt.start_auto_refresh();
    // Due at the new expiry, about 8 s after the edit; 5 min on without it.
    let deadline = std::time::Instant::now() + Duration::from_secs(60);
    while refreshes("e.json") < 2 {
        assert!(std::time::Instant::now() < deadline, "the refresh at the new expiry");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    env.finish().await;
}

/// A save the parser does not handle registers the file's built-in credential with the
/// refresh state of the credential it replaces (Go `preserveExistingAuthState`), and
/// Go's watcher then registers the written file again against it: a save that edits
/// the credential keeps its last refresh, while one that disables it ends with its own
/// fields, so the save that enables it again finds no last refresh and it is due at once.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_builtin_save_keeps_the_refresh_state_its_watcher_would_keep() {
    let Some(env) = env(
        "plugin-owned-builtin-save-deadline",
        &[("acme", "auth_provider,management_api")],
        "",
        &[(
            "s.json",
            r#"{"type":"acme","access_token":"s","refresh_interval":"1h"}"#,
        )],
        |records| {
            canned(records, "acme", "auth.parse", json!({"Handled":false}));
            canned(records, "acme", "auth.refresh", json!({"Auth":{}}));
        },
    )
    .await
    else {
        return;
    };
    let path = env.dir.join("config.yaml");
    std::fs::write(&path, &env.yaml).unwrap();
    let management = cpa_server::management::Management::new(env.rt.clone(), path);
    cpa_server::plugins::reload_plugin_auth_files(&env.rt).await;
    let refreshes = || calls(&env.records, "acme", "auth.refresh").len();
    let save = |extra: Value| {
        let mut body = json!({"type":"acme","access_token":"s","refresh_interval":"1h"});
        body.as_object_mut().unwrap().extend(extra.as_object().unwrap().clone());
        host_save(&env, "acme", "s.json", body)
    };
    env.rt.start_auto_refresh();
    eventually("the first refresh", || refreshes() == 1).await;
    // The refresh loop coalesces store changes for 1 s.
    tokio::time::sleep(Duration::from_millis(1500)).await;
    let saved = save(json!({"note":"edited"})).await;
    assert_eq!(saved["ok"], true, "{saved}");
    assert_eq!(env.rt.store().get("s.json").unwrap().metadata["note"], "edited");
    tokio::time::sleep(Duration::from_millis(2000)).await;
    assert_eq!(refreshes(), 1, "an edit keeps the last refresh");
    assert_eq!(save(json!({"disabled":true})).await["ok"], true);
    assert!(env.rt.store().get("s.json").unwrap().disabled);
    assert_eq!(save(json!({})).await["ok"], true);
    eventually("the refresh after the enabling save", || refreshes() == 2).await;
    drop(management);
    env.finish().await;
}

/// What a plugin refresh response saves (Go `MergeRefreshedAuth`, then the store): its
/// `disabled` state and the merged proxy URL reach the document, and the proxy URL the
/// running credential, so the next refresh starts from it. A response that moves the
/// credential to another provider or ID is refused; attribute changes are not applied,
/// and the refreshed tokens are saved anyway.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_plugin_refresh_saves_its_tokens_disabled_state_and_proxy_url() {
    let Some(env) = env(
        "plugin-owned-refresh-fields",
        &[("acme", "auth_provider")],
        "",
        &[
            ("d.json", r#"{"type":"acme","access_token":"d"}"#),
            (
                "x.json",
                r#"{"type":"acme","access_token":"x","proxy_url":"socks5://old:1"}"#,
            ),
            ("o.json", r#"{"type":"acme","access_token":"o"}"#),
            (
                "y.json",
                r#"{"type":"acme","access_token":"y","proxy_url":"socks5://a:1"}"#,
            ),
        ],
        |_| {},
    )
    .await
    else {
        return;
    };
    let respond = |auth: Value| {
        std::fs::write(
            env.records.join("respond/acme/auth.refresh.once.json"),
            json!({"ok":true,"result":{"Auth":auth}}).to_string(),
        )
        .unwrap();
    };
    std::fs::create_dir_all(env.records.join("respond/acme")).unwrap();
    let store = env.rt.store();

    respond(json!({"Disabled":true,"Metadata":{"type":"acme","access_token":"d2"}}));
    env.rt.refresh_credential("d.json").await.unwrap();
    assert!(store.get("d.json").unwrap().disabled);
    assert_eq!(
        auth_file(&env, "d.json"),
        json!({"type":"acme","access_token":"d2","disabled":true})
    );

    // Only the field changed, so the field's proxy URL wins.
    respond(json!({"ProxyURL":"socks5://new:2",
        "Metadata":{"type":"acme","access_token":"x2","proxy_url":"socks5://old:1"}}));
    env.rt.refresh_credential("x.json").await.unwrap();
    assert_eq!(
        auth_file(&env, "x.json"),
        json!({"type":"acme","access_token":"x2","proxy_url":"socks5://new:2","disabled":false})
    );
    assert_eq!(store.get("x.json").unwrap().attributes["proxy_url"], "socks5://new:2");
    // The next refresh, with no reload between, echoes the metadata and leaves the
    // field out: nothing changed, so the proxy URL stays the one the last refresh set.
    respond(json!({"Metadata":{"type":"acme","access_token":"x3","proxy_url":"socks5://new:2"}}));
    env.rt.refresh_credential("x.json").await.unwrap();
    assert_eq!(
        auth_file(&env, "x.json"),
        json!({"type":"acme","access_token":"x3","proxy_url":"socks5://new:2","disabled":false})
    );

    // A management edit of the proxy URL lands while the plugin refreshes and answers
    // another one: the edit wins in the document and the running credential alike.
    respond(json!({"ProxyURL":"socks5://b:2",
        "Metadata":{"type":"acme","access_token":"y2","proxy_url":"socks5://a:1"}}));
    let release = Release(env.records.join("respond/acme/auth.refresh.hold"));
    std::fs::write(&release.0, "").unwrap();
    let pending = tokio::spawn({
        let rt = env.rt.clone();
        async move { rt.refresh_credential("y.json").await }
    });
    eventually("the refresh of y.json", || {
        calls(&env.records, "acme", "auth.refresh")
            .iter()
            .any(|r| r["AuthID"] == "y.json")
    })
    .await;
    let mut edit = cpa_core::credential::MetadataPatch::default();
    edit.set.insert("proxy_url".into(), "socks5://c:3".into());
    store
        .apply_patch("y.json", store.get("y.json").unwrap().revision, &edit)
        .unwrap();
    drop(release);
    pending.await.unwrap().unwrap();
    let proxies = || {
        (
            auth_file(&env, "y.json")["proxy_url"].clone(),
            store.get("y.json").unwrap().attributes.get("proxy_url").cloned(),
        )
    };
    assert_eq!(proxies(), (json!("socks5://c:3"), Some("socks5://c:3".into())));
    // The next refresh, with no reload between, echoes its input.
    respond(json!({"Metadata":{"type":"acme","access_token":"y3","proxy_url":"socks5://c:3"}}));
    env.rt.refresh_credential("y.json").await.unwrap();
    assert_eq!(proxies(), (json!("socks5://c:3"), Some("socks5://c:3".into())));
    assert_eq!(auth_file(&env, "y.json")["access_token"], "y3");

    for (change, auth) in [
        (
            "provider from acme to other",
            json!({"Provider":"other","Metadata":{"type":"other","access_token":"o2"}}),
        ),
        (
            "ID to elsewhere",
            json!({"ID":"elsewhere","Metadata":{"type":"acme","access_token":"o2"}}),
        ),
    ] {
        respond(auth);
        let error = env.rt.refresh_credential("o.json").await.unwrap_err();
        assert!(
            String::from_utf8_lossy(&error.body)
                .contains(&format!("plugin auth refresh changed the credential's {change}")),
            "{error:?}"
        );
        assert_eq!(auth_file(&env, "o.json"), json!({"type":"acme","access_token":"o"}));
    }
    assert!(store.get("elsewhere").is_none());
    respond(json!({"Attributes":{"tier":"gold"},"Metadata":{"type":"acme","access_token":"o2"}}));
    let o = env.rt.refresh_credential("o.json").await.unwrap();
    assert_eq!(o.metadata["access_token"], "o2");
    assert!(!o.attributes.contains_key("tier"));
    assert_eq!(
        auth_file(&env, "o.json"),
        json!({"type":"acme","access_token":"o2","disabled":false})
    );
    env.finish().await;
}

/// Built-in providers keep their own refresh: a plugin auth provider registered for
/// `claude` never refreshes a Claude credential, even one with a `base_url`. A 401 and a
/// management refresh each reach Claude's token endpoint once, as without the plugin.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_claude_credential_keeps_its_builtin_refresh_beside_a_claude_auth_provider() {
    let tokens = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let token_url = {
        let tokens = tokens.clone();
        let app = axum::Router::new().route(
            "/token",
            axum::routing::post(move || {
                let n = tokens.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
                async move {
                    axum::Json(
                        json!({"access_token":format!("plain-new-{n}"),"refresh_token":format!("r{}", n + 1),
                        "expires_in":3600,"account":{"uuid":"acct-fake","email_address":"a@example.invalid"}}),
                    )
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        format!("http://{addr}/token")
    };
    let oauth = cpa_exec::oauth::OAuth::with_endpoints(wreq::Client::new(), &token_url, &token_url, &token_url);
    let Some(env) = env_with(
        "plugin-owned-refresh-claude",
        &[("claude", "auth_provider")],
        "",
        &[(
            "claude.json",
            r#"{"type":"claude","access_token":"plain-old","refresh_token":"r1","base_url":"http://127.0.0.1:1","expired":"2099-01-01T00:00:00Z"}"#,
        )],
        |records| {
            canned(
                records,
                "claude",
                "auth.refresh",
                json!({"Auth":{"Metadata":{"type":"claude","access_token":"from-plugin"}}}),
            );
        },
        Executors {
            claude: cpa_exec::claude::ClaudeExecutor::new("http://127.0.0.1:1")
                .unwrap()
                .with_oauth(oauth),
            ..executors()
        },
    )
    .await
    else {
        return;
    };
    cpa_server::plugins::reload_plugin_auth_files(&env.rt).await;
    let cfg = env.rt.config();
    let credential = env.rt.store().get("claude.json").unwrap();
    let retried = env
        .rt
        .refresh_after_unauthorized(&credential, &cfg)
        .await
        .expect("refreshed");
    assert_eq!(retried.metadata["access_token"], "plain-new-1");
    assert_eq!(tokens.load(std::sync::atomic::Ordering::SeqCst), 1);
    let forced = env.rt.refresh_credential("claude.json").await.unwrap();
    assert_eq!(forced.metadata["access_token"], "plain-new-2");
    assert_eq!(tokens.load(std::sync::atomic::Ordering::SeqCst), 2);
    assert!(calls(&env.records, "claude", "auth.refresh").is_empty());
    env.finish().await;
}

/// `executor.http_request` (M6-0114): the request goes over the wire in Go's
/// `rpcExecutorHTTPRequest` encoding (fields in declaration order, bytes as base64, the
/// callback ID last) and the plugin's `ExecutorHTTPResponse` decodes back. Go's server
/// never calls it for a plugin executor (only Codex credentials use `HttpRequest`).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn executor_http_request_crosses_the_wire_as_go_encodes_it() {
    let Some(env) = env(
        "plugin-owned-http-request",
        &[("acme", "executor")],
        "",
        &[],
        |records| {
            canned(
                records,
                "acme",
                "executor.http_request",
                json!({"StatusCode":201,"Headers":{"X-Up":["a","b"]},"Body":"aGk="}),
            );
        },
    )
    .await
    else {
        return;
    };
    let host = env.rt.plugins();
    let adapter = host.executor_adapter("acme").await.unwrap();
    let req = cpa_plugin::api::ExecutorHttpRequest {
        auth_id: "a1".into(),
        auth_provider: "acme".into(),
        method: "POST".into(),
        url: "https://up.invalid/v1/x?q=1".into(),
        headers: [("X-Test".to_owned(), vec!["1".to_owned()])].into(),
        body: bytes::Bytes::from_static(b"hello"),
        storage_json: bytes::Bytes::from_static(b"{}"),
        metadata: [("k".to_owned(), json!(1.5))].into(),
        attributes: [("path".to_owned(), "/p".to_owned())].into(),
    };
    let resp = host
        .executor_http_request(&adapter, &req, &Default::default())
        .await
        .unwrap();
    assert_eq!(resp.status_code, 201);
    assert_eq!(resp.headers["X-Up"], ["a", "b"]);
    assert_eq!(&resp.body[..], b"hi");
    let raw = std::fs::read_to_string(env.records.join("acme.jsonl")).unwrap();
    let line: Value = raw
        .lines()
        .map(|l| serde_json::from_str::<Value>(l).unwrap())
        .find(|r| r["method"] == "executor.http_request")
        .unwrap();
    let sent = line["request"].as_str().unwrap();
    let want = concat!(
        r#"{"AuthID":"a1","AuthProvider":"acme","Method":"POST","URL":"https://up.invalid/v1/x?q=1","#,
        r#""Headers":{"X-Test":["1"]},"Body":"aGVsbG8=","StorageJSON":"e30=","Metadata":{"k":1.5},"#,
        r#""Attributes":{"path":"/p"},"host_callback_id":""#
    );
    let id = sent
        .strip_prefix(want)
        .and_then(|rest| rest.strip_suffix(r#""}"#))
        .unwrap_or_else(|| panic!("{sent}"));
    assert!(!id.is_empty() && !id.contains('"'), "{sent}");
    env.finish().await;
}

/// Go 97dd9eec checks for a finish reason only once a chunk reached the handler: a stream
/// whose every chunk an interceptor drops closes before its first chunk, and Go's peek
/// loop answers that with `[DONE]`, not the truncation error.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_stream_whose_chunks_are_all_dropped_ends_with_done() {
    let upstream = {
        let sse = "data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\ndata: [DONE]\n\n";
        let handler = move || async move {
            (
                [(axum::http::header::CONTENT_TYPE, "text/event-stream")],
                sse.to_owned(),
            )
        };
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move {
            axum::serve(listener, axum::Router::new().fallback(handler))
                .await
                .unwrap()
        });
        url
    };
    let compat = format!(
        "openai-compatibility:\n  - name: up\n    base-url: {upstream}/v1\n    api-key-entries: [{{api-key: k1}}]\n    models: [{{name: up-model}}]\n"
    );
    let Some(env) = env(
        "plugin-owned-stream-drop-all",
        &[("drop", "response_stream_interceptor")],
        &compat,
        &[],
        |records| {
            canned(
                records,
                "drop",
                "response.intercept_stream_chunk",
                json!({ "DropChunk": true }),
            )
        },
    )
    .await
    else {
        return;
    };
    let (got, body) = env.chat_stream("up-model").await;
    assert_eq!(got, 200, "{body}");
    assert_eq!(body, "data: [DONE]\n\n");
    assert!(
        !calls(&env.records, "drop", "response.intercept_stream_chunk").is_empty(),
        "the interceptor saw the chunk"
    );
    env.finish().await;
}
