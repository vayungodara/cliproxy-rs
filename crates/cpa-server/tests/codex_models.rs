//! `GET /v1/models?client_version=` through the real router. The catalog builder itself
//! replays Go vectors in cpa-common; this checks the route switch and that config model
//! fields reach it. Fake keys only; nothing is sent upstream.

use std::sync::Arc;

use cpa_core::config::Config;
use cpa_exec::Executors;
use cpa_server::router;
use serde_json::Value;

async fn get(path: &str, cfg: &str) -> (u16, String, Value) {
    let cfg = Config::parse(cfg).unwrap();
    let credentials = cpa_core::config::credentials::from_config(&cfg);
    let executors = Executors {
        claude: cpa_exec::claude::ClaudeExecutor::new("http://127.0.0.1:1").unwrap(),
        codex: cpa_exec::codex::CodexExecutor::new().unwrap(),
        devices: Default::default(),
        openai: Default::default(),
        google: Default::default(),
    };
    let rt = Arc::new(cpa_server::testing::runtime(cfg, credentials, executors));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}{path}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, router(rt)).await.unwrap() });
    let response = wreq::Client::new().get(url).send().await.unwrap();
    let status = response.status().as_u16();
    let content_type = response
        .headers()
        .get("content-type")
        .map(|v| v.to_str().unwrap().to_owned())
        .unwrap_or_default();
    let body: Value = serde_json::from_slice(&response.bytes().await.unwrap()).unwrap();
    (status, content_type, body)
}

const CONFIG: &str = "codex-api-key:\n  - api-key: sk-FAKE-codex\n    base-url: http://127.0.0.1:1\n    models:\n      - name: gpt-5.5\nclaude-api-key:\n  - api-key: sk-FAKE-claude\n    models:\n      - name: claude-sonnet-4-6\n        alias: sonnet-long\n        max-context-length: 150000\nclient:\n  codex:\n    optimize-multi-agent-v2: true\n";

#[tokio::test]
async fn client_version_selects_the_codex_catalog() {
    let (status, content_type, body) = get("/v1/models?client_version=0.150.0", CONFIG).await;
    assert_eq!(status, 200);
    assert_eq!(content_type, "application/json; charset=utf-8");
    let models = body["models"].as_array().expect("Codex catalog shape");
    let slug = |s: &str| {
        models
            .iter()
            .find(|m| m["slug"] == s)
            .unwrap_or_else(|| panic!("no {s}"))
    };
    // gpt-5.5 keeps its catalog template (only Codex serves it); multi-agent v2 is on.
    let gpt = slug("gpt-5.5");
    assert_eq!(gpt["multi_agent_version"], "v2");
    assert!(gpt["base_instructions"].as_str().unwrap().len() > 1000);
    // M1-0033: the Claude key's max-context-length is the advertised window of the
    // synthesized entry, which carries compact instructions.
    let sonnet = slug("sonnet-long");
    assert_eq!(sonnet["context_window"], 150000);
    assert_eq!(sonnet["max_context_window"], 150000);
    assert_eq!(
        sonnet["base_instructions"],
        "You are Codex, a coding agent. You and the user share one workspace."
    );
    assert_eq!(sonnet["prefer_websockets"], false);
    assert!(sonnet["apply_patch_tool_type"].is_null());
    // Templates sort before synthesized entries.
    let position = |s: &str| models.iter().position(|m| m["slug"] == s).unwrap();
    assert!(position("gpt-5.5") < position("sonnet-long"));

    // An empty value still selects it; without the key the OpenAI list answers.
    let (_, _, body) = get("/v1/models?client_version", CONFIG).await;
    assert!(body["models"].is_array());
    let (_, _, body) = get("/v1/models", CONFIG).await;
    assert_eq!(body["object"], "list");
}
