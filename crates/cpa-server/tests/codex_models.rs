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

/// The same config through Go's synthesis, registration and handler
/// (tests/reference/codex_models): every client version's catalog must match byte for
/// byte, including what config models declare (explicit thinking and input modalities)
/// and the native web-search capability CPA clients see.
#[tokio::test]
async fn catalog_matches_go_for_config_models() {
    let fixture: Value = serde_json::from_str(include_str!("fixtures/codex_models_go.json")).unwrap();
    let cfg = fixture["config"].as_str().unwrap();
    for version in ["0.150.0", "cpa", "0.143.0"] {
        let (status, _, body) = get(&format!("/v1/models?client_version={version}"), cfg).await;
        assert_eq!(status, 200);
        let want: Value = serde_json::from_str(fixture[version].as_str().unwrap()).unwrap();
        let (got, want) = (body["models"].as_array().unwrap(), want["models"].as_array().unwrap());
        let slugs = |m: &[Value]| {
            m.iter()
                .map(|e| e["slug"].as_str().unwrap().to_owned())
                .collect::<Vec<_>>()
        };
        assert_eq!(slugs(got), slugs(want), "{version}: models and order");
        for (g, w) in got.iter().zip(want) {
            assert_eq!(g, w, "{version}: {}", w["slug"]);
        }
    }
}

/// upstream.claude.tool-search on (tests/reference/codex_models `TestRSFixCodexToolSearch`):
/// a Claude model advertises the search tool only when every credential serving it has
/// native tool search for its execution model (Anthropic's origin, a trusted key, not a
/// legacy model).
#[tokio::test]
async fn tool_search_catalog_matches_go() {
    let fixture: Value = serde_json::from_str(include_str!("fixtures/codex_tool_search_go.json")).unwrap();
    let (status, _, body) = get("/v1/models?client_version=0.150.0", fixture["config"].as_str().unwrap()).await;
    assert_eq!(status, 200);
    let want: Value = serde_json::from_str(fixture["0.150.0"].as_str().unwrap()).unwrap();
    let (got, want) = (body["models"].as_array().unwrap(), want["models"].as_array().unwrap());
    assert_eq!(got.len(), want.len());
    for (g, w) in got.iter().zip(want) {
        assert_eq!(g, w, "{}", w["slug"]);
    }
}

/// A Codex API key without configured models serves the Pro catalog plus the gpt-image-*
/// built-ins (Go `WithCodexBuiltins`), as Go's `/v1/models` lists them
/// (tests/reference/codex_models `TestRSFixCodexBuiltins`). Compared as a set: list order
/// is the registry's concern.
#[tokio::test]
async fn codex_builtins_match_go_models_list() {
    let fixture: Value = serde_json::from_str(include_str!("fixtures/codex_builtins_go.json")).unwrap();
    let (status, _, body) = get("/v1/models", fixture["config"].as_str().unwrap()).await;
    assert_eq!(status, 200);
    let want: Value = serde_json::from_str(fixture["models"].as_str().unwrap()).unwrap();
    let sorted = |list: &Value| {
        let mut entries = list["data"].as_array().unwrap().clone();
        entries.sort_by(|a, b| a["id"].as_str().cmp(&b["id"].as_str()));
        entries
    };
    let (got, want) = (sorted(&body), sorted(&want));
    assert!(want.iter().any(|m| m["id"] == "gpt-image-2"), "Go lists the built-ins");
    assert_eq!(got, want);
}

/// M1-0128 `GET /v1/models/*model`: the entry from the catalog this client would list,
/// and Go's JSON 404 for an unknown or empty ID.
#[tokio::test]
async fn model_detail_comes_from_the_clients_catalog() {
    let (_, _, codex) = get("/v1/models?client_version=0.150.0", CONFIG).await;
    let (status, content_type, body) = get("/v1/models/sonnet-long?client_version=0.150.0", CONFIG).await;
    assert_eq!(
        (status, content_type.as_str()),
        (200, "application/json; charset=utf-8")
    );
    let listed = codex["models"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["slug"] == "sonnet-long");
    assert_eq!(Some(&body), listed);
    let (_, _, openai) = get("/v1/models", CONFIG).await;
    let (status, _, mut body) = get("/v1/models/sonnet-long", CONFIG).await;
    assert_eq!(status, 200);
    let mut listed = openai["data"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["id"] == "sonnet-long")
        .unwrap()
        .clone();
    // A config model is `created` at the request's second; the two requests can
    // straddle a second boundary.
    let (got, want) = (body["created"].take(), listed["created"].take());
    assert!(got.as_i64().unwrap() - want.as_i64().unwrap() <= 1, "{got} {want}");
    assert!(got.as_i64() >= want.as_i64(), "{got} {want}");
    assert_eq!(body, listed);
    assert!(body.get("slug").is_none(), "the OpenAI entry, not the Codex one");
    let missing = serde_json::json!({"error": {"code": "model_not_found", "message": "Model not found", "type": "invalid_request_error"}});
    for path in ["/v1/models/nope", "/v1/models/", "/v1/models/sonnet-long/extra"] {
        let (status, content_type, body) = get(path, CONFIG).await;
        assert_eq!(
            (status, content_type.as_str(), &body),
            (404, "application/json; charset=utf-8", &missing),
            "{path}"
        );
    }
}

/// Go `TestIssue5190ModelDetailLifecycle` and `TestIssue5190ModelDetailUsesVisibleCatalog`:
/// the detail is picked from the catalog a response interceptor returned (its stale
/// `Content-Length` dropped), and the request lifecycle completes once with the detail's
/// status. The plugin is the Go recorder test plugin.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn model_detail_reads_the_intercepted_catalog() {
    use base64::Engine as _;
    let tmp = std::path::Path::new(env!("CARGO_TARGET_TMPDIR"));
    let Some(built) = cpa_plugin::testing::built_plugins(tmp, "model_detail_interceptor") else {
        return;
    };
    let dir = cpa_plugin::testing::scratch(tmp, "model-detail");
    let (auths, plugins, records) = (dir.join("auths"), dir.join("plugins"), dir.join("records"));
    let respond = records.join("respond/catalog");
    for d in [&auths, &plugins, &respond] {
        std::fs::create_dir_all(d).unwrap();
    }
    std::fs::copy(built.join("recorder.so"), plugins.join("catalog.so")).unwrap();
    let yaml = format!(
        "auth-dir: {}\nplugins:\n  enabled: true\n  dir: {}\n  configs:\n    catalog:\n      enabled: true\n      label: catalog\n      record: {}\n      caps: response_interceptor,request_lifecycle_plugin\n",
        auths.display(),
        plugins.display(),
        records.display()
    );
    let executors = Executors {
        claude: cpa_exec::claude::ClaudeExecutor::new("http://127.0.0.1:1").unwrap(),
        codex: Default::default(),
        devices: Default::default(),
        openai: Default::default(),
        google: Default::default(),
    };
    let rt = Arc::new(cpa_server::testing::runtime(
        Config::parse(&yaml).unwrap(),
        vec![],
        executors,
    ));
    cpa_server::plugins::start(&rt).await;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, router(rt)).await.unwrap() });
    let completions = || -> Vec<Value> {
        std::fs::read_to_string(records.join("catalog.jsonl"))
            .unwrap_or_default()
            .lines()
            .map(|line| serde_json::from_str::<Value>(line).unwrap())
            .filter(|record| record["method"] == "request.complete")
            .map(|record| serde_json::from_str(record["request"].as_str().unwrap()).unwrap())
            .collect()
    };
    let catalog = r#"{"object":"list","data":[{"id":"vendor/visible","object":"model","owned_by":"plugin"}]}"#;
    for (body, id, status, outcome) in [
        (catalog, "vendor/visible", 200, "succeeded"),
        (catalog, "hidden", 404, "failed"),
        (r#"{"data":[]}"#, "vendor/visible", 404, "failed"),
        ("invalid", "vendor/visible", 502, "failed"),
    ] {
        let intercepted = serde_json::json!({"Headers": {"Content-Length": ["98"]},
            "Body": base64::engine::general_purpose::STANDARD.encode(body)});
        std::fs::write(
            respond.join("response.intercept_after.json"),
            serde_json::json!({"ok": true, "result": intercepted}).to_string(),
        )
        .unwrap();
        let before = completions().len();
        let response = wreq::Client::new()
            .get(format!("{base}/v1/models/{id}"))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status().as_u16(), status, "{id} in {body}");
        let text = response.text().await.unwrap();
        if status == 200 {
            assert_eq!(text, r#"{"id":"vendor/visible","object":"model","owned_by":"plugin"}"#);
        }
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while completions().len() == before {
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        let done = completions();
        assert_eq!(done.len(), before + 1, "one completion per detail request");
        let last = &done[before];
        assert_eq!(
            (last["StatusCode"].as_i64(), last["Outcome"].as_str()),
            (Some(i64::from(status)), Some(outcome)),
            "{id} in {body}"
        );
    }
}
