//! Remote model catalog refresh against local mock sources. Its own test binary: the
//! refresh swaps the process-wide static catalog.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use cpa_core::registry::{catalog_generation, pinned};

const EMBEDDED: &str = include_str!("../../cpa-core/src/registry/models.json");

async fn serve(app: axum::Router) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    url
}

/// A Gemini model only Go's typed decode reads as intended: nulls in a string list and a
/// header map, HTML characters, the integer `-0` and unknown members.
const TYPED_GEMINI: &str = r#"{"id":"gemini-typed","inputTokenLimit":123,"supportedInputModalities":[null,"text"],"config":{"override_header":{"X-Test":null,"b":"<v>"}},"thinking":{"min":1,"levels":["low"],"disable_type":"x"},"display_name":"D","created":-0,"native_capabilities":{"web_search":true},"unknown":{"a":[1.5]}}"#;

/// Go 1.26.4 `json.Marshal` of [`TYPED_GEMINI`] decoded into `registry.ModelInfo`.
const TYPED_GEMINI_GO: &str = r#"{"id":"gemini-typed","object":"","created":0,"owned_by":"","type":"","display_name":"D","inputTokenLimit":123,"supportedInputModalities":["","text"],"thinking":{"min":1,"disable_type":"x","levels":["low"]},"config":{"override_header":{"X-Test":"","b":"\u003cv\u003e"}}}"#;

/// Catalogs refused whole (docs/DIFFERENCES-FROM-GO.md, remote model catalogs), as raw
/// text: a parsed `Value` cannot hold repeated members. Each adds one Gemini model (or a
/// section) to the otherwise acceptable refreshed catalog. Go refuses the integer-field
/// fractions, `"bad"` then `123` (also as a repeated header name), and the empty decoded
/// ID; it serves `""` for a null `type` and level and merges the others differently from
/// the decoded map.
const REFUSED: [(&str, &str); 17] = [
    (
        "repeated_header_bad_first",
        r#"{"id":"g","config":{"override_header":{"X-Test":123,"X-Test":"ok"}}}"#,
    ),
    ("zero_fraction", r#"{"id":"g","inputTokenLimit":0.0}"#),
    ("negative_zero_fraction", r#"{"id":"g","inputTokenLimit":-0.0}"#),
    ("zero_exponent", r#"{"id":"g","inputTokenLimit":-0e0}"#),
    ("underflow", r#"{"id":"g","inputTokenLimit":1e-400}"#),
    ("thinking_fraction", r#"{"id":"g","thinking":{"max":-0.0}}"#),
    (
        "repeated_in_two_cases",
        r#"{"id":"g","inputTokenLimit":1,"InputTokenLimit":2,"inputTokenLimit":3}"#,
    ),
    (
        "repeated_after_bad",
        r#"{"id":"g","inputTokenLimit":"bad","inputTokenLimit":123}"#,
    ),
    (
        "repeated_list",
        r#"{"id":"g","supported_parameters":["a","b"],"supported_parameters":["c"]}"#,
    ),
    (
        "repeated_thinking",
        r#"{"id":"g","thinking":{"min":1},"thinking":{"max":2}}"#,
    ),
    (
        "repeated_native",
        r#"{"id":"g","native_capabilities":{"web_search":"x","web_search":true}}"#,
    ),
    ("thinking_member_case", r#"{"id":"g","thinking":{"Min":1}}"#),
    ("type_null", r#"{"id":"g","type":null}"#),
    ("levels_null", r#"{"id":"g","thinking":{"levels":[null]}}"#),
    ("empty_decoded_id", r#"{"id":"g","ID":""}"#),
    ("section_in_two_cases", r#"SECTION"GEMINI":[{"id":"b"}]"#),
    ("section_in_another_case", r#"LONE"GEMINI":[{"id":"b"}]"#),
];

/// An accepted Gemini model with `-0` in an integer field (Go's 0, serde's `-0.0`) and a
/// repeated header name (Go keeps the last value).
const LENIENT: &str = r#"{"id":"g-lenient","type":"gemini","owned_by":"google","context_length":4096,"thinking":{"min":-0,"max":1024},"config":{"override_header":{"X-Test":"a","X-Test":"ok","x-test":null}}}"#;

/// The refreshed catalog with `case` from [`REFUSED`] added, its model ID made unique so
/// that one wrongly accepted case cannot hide the next.
fn refused(modified: &str, name: &str, case: &str) -> String {
    let case = &case.replace(r#"{"id":"g""#, &format!(r#"{{"id":"g-{name}""#));
    if let Some(section) = case.strip_prefix("SECTION") {
        // After the `gemini` section, so Go decodes this one last.
        return format!("{},{section}}}", modified.strip_suffix('}').unwrap());
    }
    if let Some(section) = case.strip_prefix("LONE") {
        // In place of the `gemini` section, which comes right before `vertex`.
        let gemini = modified.find(r#""gemini":["#).unwrap();
        let vertex = modified.find(r#""vertex":["#).unwrap();
        return format!("{}{section},{}", &modified[..gemini], &modified[vertex..]);
    }
    modified.replacen(r#""gemini":["#, &format!(r#""gemini":[{case},"#), 1)
}

/// The embedded catalog with one extra Claude model and a placeholder for
/// [`TYPED_GEMINI`], which [`render`] fills in.
fn with_extra_claude_model() -> serde_json::Map<String, serde_json::Value> {
    let mut root: serde_json::Map<String, serde_json::Value> = serde_json::from_str(EMBEDDED).unwrap();
    let mut extra = root["claude"][0].as_object().unwrap().clone();
    extra.insert("id".into(), "claude-refreshed-1".into());
    root["claude"].as_array_mut().unwrap().push(extra.into());
    root["gemini"]
        .as_array_mut()
        .unwrap()
        .push(serde_json::json!({"id": "TYPED_GEMINI"}));
    root
}

/// `root` as text with [`TYPED_GEMINI`] as written: serde would write its `-0` as `-0.0`.
fn render(root: &serde_json::Map<String, serde_json::Value>) -> String {
    serde_json::to_string(root)
        .unwrap()
        .replacen(r#"{"id":"TYPED_GEMINI"}"#, TYPED_GEMINI, 1)
}

#[tokio::test]
async fn refresh_skips_failing_sources_and_swaps_a_changed_catalog() {
    let hits = Arc::new(AtomicUsize::new(0));
    let root = with_extra_claude_model();
    let modified = render(&root);
    let invalid = r#"{"claude":[{"id":"a"},{"id":"a"}]}"#.to_owned();
    // Go's typed decode refuses a fractional token limit.
    let mistyped = {
        let mut root = root.clone();
        root["gemini"][0]["inputTokenLimit"] = serde_json::json!(1.5);
        render(&root)
    };
    let lenient = modified.replacen(r#""gemini":["#, &format!(r#""gemini":[{LENIENT},"#), 1);
    let refused_cases: Vec<(&str, String)> = REFUSED.iter().map(|(n, c)| (*n, refused(&modified, n, c))).collect();
    let without_meta = {
        let mut root = root.clone();
        root.insert("meta".into(), serde_json::Value::Array(Vec::new()));
        render(&root)
    };
    let app = axum::Router::new()
        .route(
            "/{name}",
            axum::routing::get(move |State(hits): State<Arc<AtomicUsize>>, Path(name): Path<String>| {
                let (modified, invalid, mistyped, without_meta, refused_cases, lenient) = (
                    modified.clone(),
                    invalid.clone(),
                    mistyped.clone(),
                    without_meta.clone(),
                    refused_cases.clone(),
                    lenient.clone(),
                );
                async move {
                    hits.fetch_add(1, Ordering::SeqCst);
                    match name.as_str() {
                        "down" => (StatusCode::INTERNAL_SERVER_ERROR, String::new()).into_response(),
                        "invalid" => invalid.into_response(),
                        "mistyped" => mistyped.into_response(),
                        "modified" => modified.into_response(),
                        "lenient" => lenient.into_response(),
                        name if name.starts_with("refused-") => {
                            let case = refused_cases.iter().find(|(n, _)| name[8..] == **n).unwrap();
                            case.1.clone().into_response()
                        }
                        _ => without_meta.into_response(),
                    }
                }
            }),
        )
        .with_state(hits.clone());
    let base = serve(app).await;
    let urls = |names: &[&str]| names.iter().map(|n| format!("{base}/{n}")).collect::<Vec<_>>();

    assert!(pinned().lookup("claude-refreshed-1").is_none());
    let mut metadata = serde_json::Map::new();
    metadata.insert("type".into(), "claude".into());
    metadata.insert("access_token".into(), "fake-token".into());
    let credential = cpa_core::credential::Credential::from_file(
        std::path::Path::new("/auth"),
        std::path::Path::new("/auth/c.json"),
        metadata,
    )
    .unwrap();
    let mut metadata = serde_json::Map::new();
    metadata.insert("type".into(), "gemini".into());
    metadata.insert("access_token".into(), "fake-gemini-token".into());
    let gemini = cpa_core::credential::Credential::from_file(
        std::path::Path::new("/auth"),
        std::path::Path::new("/auth/g.json"),
        metadata,
    )
    .unwrap();
    let gemini_id = gemini.id.clone();
    let executors = cpa_exec::Executors {
        claude: cpa_exec::claude::ClaudeExecutor::new("http://127.0.0.1:9").unwrap(),
        codex: Default::default(),
        devices: Default::default(),
        openai: Default::default(),
        google: Default::default(),
    };
    let auth = std::env::temp_dir().join(format!("model-updater-{}", std::process::id()));
    std::fs::create_dir_all(&auth).unwrap();
    let config = auth.join("config.yaml");
    let hash = bcrypt::hash("fake-management-only", 4).unwrap();
    let yaml = format!(
        "api-keys:\n  - fake-client-key\nmanagement:\n  secret-key: '{hash}'\noauth:\n  auth-dir: {}\n",
        auth.display()
    );
    std::fs::write(&config, yaml).unwrap();
    let rt = Arc::new(cpa_server::testing::runtime(
        cpa_core::config::Config::load(&config).unwrap(),
        vec![credential, gemini],
        executors,
    ));
    let registered = |rt: &cpa_server::Runtime| rt.registry().ids().any(|id| id == "claude-refreshed-1");
    assert!(!registered(&rt));
    let management = cpa_server::management::Management::new(rt.clone(), config);
    let app = cpa_server::app(rt.clone(), cpa_server::management::router(management));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(
            listener,
            app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
        )
        .await
        .unwrap()
    });
    let served = |addr: std::net::SocketAddr| async move {
        wreq::Client::new()
            .get(format!("http://{addr}/v8/management/routing/model-definitions/gemini"))
            .bearer_auth("fake-management-only")
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap()
    };
    let meta_before = pinned().channel("meta").len();
    let generation = catalog_generation();

    // Every source fails: the current catalog stays.
    cpa_server::model_updater::refresh(&urls(&["down", "invalid", "mistyped"])).await;
    assert_eq!(catalog_generation(), generation);

    // Each refused catalog leaves the catalog and the served definitions as they were.
    let mut accepted = Vec::new();
    for (name, _) in REFUSED {
        let (before, served_before) = (catalog_generation(), served(addr).await);
        cpa_server::model_updater::refresh(&urls(&[&format!("refused-{name}")])).await;
        if catalog_generation() != before || served(addr).await != served_before {
            accepted.push(name);
        }
    }
    assert!(accepted.is_empty(), "accepted: {accepted:?}");

    // A 500 and an invalid catalog are skipped; the third source is applied.
    cpa_server::model_updater::refresh(&urls(&["down", "invalid", "modified"])).await;
    assert_eq!(hits.load(Ordering::SeqCst), 6 + REFUSED.len());
    assert_eq!(catalog_generation(), generation + 1);
    assert!(pinned().lookup("claude-refreshed-1").is_some());
    // Management's model definitions read the refreshed catalog.
    let definitions = cpa_core::registry::dynamic::static_definitions("claude").unwrap();
    assert_eq!(definitions.last().unwrap().id, "claude-refreshed-1");
    // ... and render a refreshed model as Go's typed decode and encode do.
    let body = served(addr).await;
    assert!(body.ends_with(&format!(",{TYPED_GEMINI_GO}]}}")), "{body}");
    std::fs::remove_dir_all(&auth).unwrap();
    // Go's refresh callback re-registers the credentials of changed providers.
    assert!(registered(&rt));

    // The same content again changes nothing.
    cpa_server::model_updater::refresh(&urls(&["modified"])).await;
    assert_eq!(catalog_generation(), generation + 1);

    // An empty remote `meta` section keeps the current one (Go tryRefreshModels).
    cpa_server::model_updater::refresh(&urls(&["no-meta"])).await;
    assert_eq!(catalog_generation(), generation + 1, "identical once meta is kept");
    assert_eq!(pinned().channel("meta").len(), meta_before);

    // `-0` is Go's 0 in the static catalog, the Gemini credential's registration, the
    // client model list and the management rendering; the last of repeated header names
    // wins.
    cpa_server::model_updater::refresh(&urls(&["lenient"])).await;
    assert_eq!(catalog_generation(), generation + 2);
    let thinking = |min, max| {
        Some(cpa_core::registry::ThinkingSupport {
            min,
            max,
            ..Default::default()
        })
    };
    let model = pinned().lookup("g-lenient").unwrap();
    assert_eq!(model.thinking, thinking(0, 1024));
    let spec = rt.registry().client_model(&gemini_id, "g-lenient").expect("registered");
    assert_eq!(
        (spec.owned_by.as_str(), spec.context_length, spec.thinking.clone()),
        ("google", 4096, thinking(0, 1024))
    );
    let models: serde_json::Value = wreq::Client::new()
        .get(format!("http://{addr}/v1/models"))
        .bearer_auth("fake-client-key")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let listed = models["data"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["id"] == "g-lenient");
    assert_eq!(listed.expect("listed")["owned_by"], "google", "{models}");
    // Go 1.26.4 json.Marshal of LENIENT decoded into registry.ModelInfo.
    let go = r#"{"id":"g-lenient","object":"","created":0,"owned_by":"google","type":"gemini","context_length":4096,"thinking":{"max":1024},"config":{"override_header":{"X-Test":"ok","x-test":""}}}"#;
    let body = served(addr).await;
    assert!(body.contains(go), "{body}");
}
