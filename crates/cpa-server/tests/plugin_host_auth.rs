//! `host.auth.*` and `host.affinity.lookup` against the server's credential store, as
//! Go's plugin host answers them once `SetAuthManager` gave it the auth manager
//! (internal/pluginhost/auth_callbacks.go, affinity_callbacks.go). The callback logic
//! itself is checked against Go's host in cpa-plugin's golden; this covers the
//! store's side: indexes, file-backed entries, saves that register at once, and
//! session bindings made by the scheduler.

use std::sync::Arc;

use cpa_core::config::Config;
use cpa_exec::Executors;
use cpa_exec::claude::ClaudeExecutor;
use cpa_plugin::callbacks::RequestScope;
use cpa_plugin::client::CallbackInstance;
use cpa_plugin::testing::{call_from_plugin, callback_context};
use cpa_server::management::{Management, Options};
use cpa_server::runtime::Selection;
use serde_json::Value;

fn result(raw: &[u8]) -> Value {
    let envelope: Value = serde_json::from_slice(raw).unwrap();
    assert_eq!(envelope["ok"], true, "{envelope}");
    envelope["result"].clone()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn plugin_auth_callbacks_read_and_save_the_credential_store() {
    let work = cpa_plugin::testing::scratch(std::path::Path::new(env!("CARGO_TARGET_TMPDIR")), "plugin-host-auth");
    let auths = work.join("auths");
    std::fs::create_dir_all(&auths).unwrap();
    // Fake credentials; nothing is sent anywhere.
    std::fs::write(
        auths.join("claude.json"),
        r#"{"type":"claude","email":" a@example.invalid ","access_token":"fake-access","priority":"5"}"#,
    )
    .unwrap();
    std::fs::write(
        auths.join("codex.json"),
        r#"{"type":"codex","email":"c@example.invalid","disabled":true}"#,
    )
    .unwrap();
    let config_path = work.join("config.yaml");
    std::fs::write(
        &config_path,
        format!(
            "config-version: 8\nauth-dir: {}\nrouting:\n  session-affinity: true\n",
            auths.display()
        ),
    )
    .unwrap();
    let config = Config::load(&config_path).unwrap();
    let credentials = cpa_core::config::credentials::load(&config);
    let claude = credentials.iter().find(|c| c.id == "claude.json").unwrap().clone();
    let index = cpa_core::config::credentials::auth_index(&claude);
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
    let _state = Management::with_options(rt.clone(), config_path, Options::default());
    // The host learns the config (and so the auth directory) as the server starts.
    cpa_server::plugins::start(&rt).await;
    let host = rt.plugins().clone();
    let instance = Arc::new(CallbackInstance::default());
    let guard = callback_context(&host, "p", instance.clone(), RequestScope::default());

    // A session the scheduler bound to the Claude credential.
    let mut selection = Selection::new("claude", "claude-sonnet-4(high)");
    selection.session = Some("s1".into());
    let lease = rt.store().select(selection).expect("the claude credential is picked");
    drop(lease);

    let (h, i, auths_dir, idx) = (host.clone(), instance.clone(), auths.clone(), index.clone());
    tokio::task::spawn_blocking(move || {
        let call = |method: &str, request: &str| call_from_plugin(&h, "p", &i, method, request.as_bytes());

        let list = result(&call("host.auth.list", "{}").unwrap());
        let files = list["files"].as_array().unwrap();
        let names: Vec<&str> = files.iter().map(|f| f["name"].as_str().unwrap()).collect();
        assert_eq!(names, ["claude.json", "codex.json"]);
        let entry = &files[0];
        assert_eq!(entry["auth_index"], idx.as_str());
        assert_eq!(entry["id"], "claude.json");
        assert_eq!(entry["type"], "claude");
        assert_eq!(entry["status"], "active");
        assert_eq!(entry["source"], "file");
        assert_eq!(entry["email"], "a@example.invalid");
        assert_eq!(entry["account_type"], "oauth");
        assert_eq!(entry["account"], "a@example.invalid");
        assert_eq!(entry["priority"], 5);
        assert_eq!(entry["path"], auths_dir.join("claude.json").display().to_string());
        assert_eq!(entry["recent_requests"].as_array().unwrap().len(), 20);
        assert_eq!(files[1]["status"], "disabled");
        assert_eq!(files[1]["disabled"], true);

        let got = result(&call("host.auth.get", &format!(r#"{{"auth_index":"{idx}"}}"#)).unwrap());
        assert_eq!(got["name"], "claude.json");
        assert_eq!(got["json"]["access_token"], "fake-access");

        // A save registers the file at once (Go's upsertAuthRecord).
        let saved = result(
            &call(
                "host.auth.save",
                r#"{"name":"new.json","json":{"type":"claude","email":"n@example.invalid"}}"#,
            )
            .unwrap(),
        );
        assert_eq!(saved["path"], auths_dir.join("new.json").display().to_string());
        let list = result(&call("host.auth.list", "{}").unwrap());
        let new = list["files"]
            .as_array()
            .unwrap()
            .iter()
            .find(|f| f["name"] == "new.json")
            .expect("the saved credential is listed")
            .clone();
        let runtime = result(
            &call(
                "host.auth.get_runtime",
                &format!(r#"{{"auth_index":"{}"}}"#, new["auth_index"].as_str().unwrap()),
            )
            .unwrap(),
        );
        assert_eq!(runtime["auth"]["label"], "n@example.invalid");

        // Go's file store persists the auth's own disabled flag as a boolean and the
        // credential registers disabled; a file the loader would skip still registers.
        result(
            &call(
                "host.auth.save",
                r#"{"name":"off.json","json":{"type":"claude","disabled":"true","base-url":"https://x.invalid"}}"#,
            )
            .unwrap(),
        );
        let persisted: Value = serde_json::from_slice(&std::fs::read(auths_dir.join("off.json")).unwrap()).unwrap();
        assert_eq!(
            persisted,
            serde_json::json!({"type":"claude","disabled":true,"base_url":"https://x.invalid"})
        );
        result(&call("host.auth.save", r#"{"name":"bare.json","json":{}}"#).unwrap());
        let list = result(&call("host.auth.list", "{}").unwrap());
        let by_name = |name: &str| {
            list["files"]
                .as_array()
                .unwrap()
                .iter()
                .find(|f| f["name"] == name)
                .unwrap_or_else(|| panic!("{name} is listed"))
                .clone()
        };
        assert_eq!(by_name("off.json")["disabled"], true);
        assert_eq!(by_name("off.json")["status"], "disabled");
        assert_eq!(by_name("bare.json")["type"], "unknown");

        // Session affinity: the thinking suffix is not part of the binding; a session
        // with a known prefix is looked up as is.
        let lookup = |provider: &str, model: &str, session: &str| {
            result(
                &call(
                    "host.affinity.lookup",
                    &format!(r#"{{"provider":"{provider}","model":"{model}","session_id":"{session}"}}"#),
                )
                .unwrap(),
            )
        };
        let bound = lookup("claude", "claude-sonnet-4(low)", "s1");
        assert_eq!(bound["status"], "bound");
        assert_eq!(bound["auth_index"], idx.as_str());
        assert!(bound.get("disabled").is_none() && bound.get("unavailable").is_none());
        assert_eq!(lookup("claude", "claude-sonnet-4", "other")["status"], "unbound");
        assert_eq!(lookup("claude", "claude-sonnet-4", "header:s1")["status"], "unbound");
        assert_eq!(lookup("codex", "claude-sonnet-4", "s1")["status"], "unbound");
    })
    .await
    .unwrap();

    // Without session affinity Go's selector cannot observe bindings.
    let mut cfg = (*rt.config()).clone();
    cfg.routing.session_affinity = false;
    rt.publish_policy(cpa_server::management::policy(&cfg));
    let (h, i) = (host.clone(), instance.clone());
    tokio::task::spawn_blocking(move || {
        let lookup = call_from_plugin(
            &h,
            "p",
            &i,
            "host.affinity.lookup",
            br#"{"provider":"claude","model":"claude-sonnet-4","session_id":"s1"}"#,
        )
        .unwrap();
        assert_eq!(result(&lookup)["status"], "unsupported");
    })
    .await
    .unwrap();
    drop(guard);
    let _ = std::fs::remove_dir_all(&work);
}

/// A binding is keyed by the credential's scheduling key; for an OpenAI-compatible
/// credential that is its `provider_key`, not its raw provider.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn affinity_lookup_finds_custom_provider_bindings() {
    let work = cpa_plugin::testing::scratch(
        std::path::Path::new(env!("CARGO_TARGET_TMPDIR")),
        "plugin-affinity-compat",
    );
    let auths = work.join("auths");
    std::fs::create_dir_all(&auths).unwrap();
    let config_path = work.join("config.yaml");
    std::fs::write(
        &config_path,
        format!(
            "config-version: 8\nauth-dir: {}\nrouting:\n  session-affinity: true\n",
            auths.display()
        ),
    )
    .unwrap();
    let config = Config::load(&config_path).unwrap();
    let mut meta = serde_json::Map::new();
    meta.insert("type".into(), "openai-compatibility".into());
    let mut compat = cpa_core::credential::Credential::from_file(&auths, &auths.join("acme.json"), meta).unwrap();
    compat.attributes.insert("provider_key".into(), "acme".into());
    assert_eq!(compat.provider, "openai-compatibility");
    let index = cpa_core::config::credentials::auth_index(&compat);
    let rt = Arc::new(cpa_server::testing::runtime(
        config,
        vec![compat],
        Executors {
            claude: ClaudeExecutor::new("http://127.0.0.1:1").unwrap(),
            codex: Default::default(),
            devices: Default::default(),
            openai: Default::default(),
            google: Default::default(),
        },
    ));
    let _state = Management::with_options(rt.clone(), config_path, Options::default());
    cpa_server::plugins::start(&rt).await;
    let host = rt.plugins().clone();
    let instance = Arc::new(CallbackInstance::default());
    let guard = callback_context(&host, "p", instance.clone(), RequestScope::default());
    let mut selection = Selection::new("acme", "up-model");
    selection.session = Some("s1".into());
    drop(rt.store().select(selection).expect("the compat credential is picked"));
    let (h, i) = (host.clone(), instance.clone());
    tokio::task::spawn_blocking(move || {
        let lookup = |provider: &str| {
            let request = format!(r#"{{"provider":"{provider}","model":"up-model","session_id":"s1"}}"#);
            result(&call_from_plugin(&h, "p", &i, "host.affinity.lookup", request.as_bytes()).unwrap())
        };
        let bound = lookup("acme");
        assert_eq!(bound["status"], "bound", "{bound}");
        assert_eq!(bound["auth_index"], index.as_str());
        assert_eq!(lookup("openai-compatibility")["status"], "unbound");
    })
    .await
    .unwrap();
    drop(guard);
    let _ = std::fs::remove_dir_all(&work);
}
