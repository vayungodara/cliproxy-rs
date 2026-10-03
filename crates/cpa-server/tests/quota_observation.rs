//! Go's manager-level quota observation cases (sdk/cliproxy/auth/quota_signals_test.go)
//! through the real router, scheduler and Claude executor against a loopback upstream,
//! read back from the management credential list. Observations live in the executors
//! and cooldowns in the scheduler; the Go cases that guard one from the other check that
//! neither path writes the other's state. Fake keys only.

use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use axum::extract::{Request, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use cpa_core::config::Config;
use cpa_core::credential::Credential;
use cpa_exec::Executors;
use cpa_exec::claude::ClaudeExecutor;
use cpa_server::cooldown_store::{Backend, Quota, Record};
use cpa_server::management::{Management, Options};
use serde_json::{Value, json};

const PASSWORD: &str = "fake-management-password";
const OPUS: &str = "claude-opus-4-6";
const SONNET: &str = "claude-sonnet-4-6";

/// Models of the Messages requests the upstream answered.
type Seen = Arc<Mutex<Vec<String>>>;

/// Answers per model, with the quota headers a real Claude response carries.
async fn upstream(State(seen): State<Seen>, req: Request) -> Response {
    let raw = axum::body::to_bytes(req.into_body(), usize::MAX).await.unwrap();
    let body: Value = serde_json::from_slice(&raw).unwrap();
    let model = body["model"].as_str().unwrap_or_default().to_owned();
    seen.lock().unwrap().push(model.clone());
    let reply = r#"{"id":"msg_1","type":"message","role":"assistant","content":[],"model":"m","stop_reason":"end_turn","usage":{"input_tokens":1,"output_tokens":1}}"#;
    if raw.windows(8).any(|w| w == b"MODE_429") {
        // A rejected shared window: a credential-scope quota failure.
        return (
            StatusCode::TOO_MANY_REQUESTS,
            [
                ("content-type", "application/json"),
                ("retry-after", "120"),
                ("anthropic-ratelimit-unified-5h-status", "rejected"),
                ("anthropic-ratelimit-unified-5h-utilization", "0.99"),
            ],
            r#"{"type":"error","error":{"type":"rate_limit_error","message":"quota"}}"#,
        )
            .into_response();
    }
    let utilization = if model == OPUS { "0.40" } else { "0.41" };
    (
        [
            ("content-type", "application/json"),
            ("anthropic-ratelimit-unified-5h-utilization", utilization),
            ("anthropic-workspace-id", "ws-not-a-signal"),
        ],
        reply,
    )
        .into_response()
}

async fn serve(app: axum::Router) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    url
}

fn credential(file: &str, meta: Value, attrs: &[(&str, &str)]) -> Credential {
    let mut c = Credential::from_file(
        Path::new("/fake"),
        &Path::new("/fake").join(file),
        meta.as_object().unwrap().clone(),
    )
    .unwrap();
    for (k, v) in attrs {
        c.attributes.insert((*k).into(), (*v).into());
    }
    c
}

struct Proxy {
    url: String,
    rt: Arc<cpa_server::Runtime>,
    seen: Seen,
    _dir: std::path::PathBuf,
}

async fn proxy(name: &str) -> Proxy {
    let seen: Seen = Arc::default();
    let upstream_url = serve(axum::Router::new().fallback(upstream).with_state(seen.clone())).await;
    let credentials = vec![
        credential(
            "claude-q.json",
            json!({"type":"claude"}),
            &[("api_key", "sk-fake-claude"), ("base_url", &upstream_url)],
        ),
        credential("kimi-k.json", json!({"type":"kimi"}), &[]),
        credential("devin-d.json", json!({"type":"devin"}), &[]),
    ];
    let yaml = "api-keys:\n  - client-key\n";
    let rt = Arc::new(cpa_server::testing::runtime(
        Config::parse(yaml).unwrap(),
        credentials,
        Executors {
            claude: ClaudeExecutor::new("http://127.0.0.1:1").unwrap(),
            codex: Default::default(),
            devices: Default::default(),
            openai: Default::default(),
            google: Default::default(),
        },
    ));
    let dir = std::env::temp_dir().join(format!("cpa-quota-{name}-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("config.yaml");
    std::fs::write(&path, yaml).unwrap();
    let options = Options {
        management_password: Some(PASSWORD.into()),
        ..Default::default()
    };
    let management = Management::with_options(rt.clone(), path, options);
    let app = cpa_server::app(rt.clone(), cpa_server::management::router(management));
    Proxy {
        url: serve(app).await,
        rt,
        seen,
        _dir: dir,
    }
}

impl Proxy {
    async fn message(&self, model: &str, text: &str) -> u16 {
        let body = json!({"model": model, "max_tokens": 8, "messages": [{"role": "user", "content": text}]});
        wreq::Client::new()
            .post(format!("{}/v1/messages", self.url))
            .header("x-api-key", "client-key")
            .header("content-type", "application/json")
            .body(body.to_string())
            .send()
            .await
            .unwrap()
            .status()
            .as_u16()
    }

    /// The management entry of `file` (Go `buildAuthFileEntry`).
    async fn entry(&self, file: &str) -> Value {
        let listed: Value = wreq::Client::new()
            .get(format!("{}/v0/management/auth-files", self.url))
            .bearer_auth(PASSWORD)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        listed["files"]
            .as_array()
            .unwrap_or_else(|| panic!("{listed}"))
            .iter()
            .find(|e| e["name"] == file)
            .unwrap_or_else(|| panic!("{file} in {listed}"))
            .clone()
    }

    fn upstream_calls(&self, model: &str) -> usize {
        self.seen.lock().unwrap().iter().filter(|m| *m == model).count()
    }
}

/// TestManagerMarkResultRecordsResponseQuotaSignalsInMemory,
/// TestMarkResultQuotaFailureDoesNotEraseSiblingObservation,
/// TestApplyCooldownFieldsPreservesObservation and
/// TestCooldownEqualityIgnoresObservationSignals, for a Claude credential.
#[tokio::test]
async fn results_observe_the_credential_and_their_model_and_cooldowns_never_touch_them() {
    let p = proxy("results").await;
    assert_eq!(p.message(OPUS, "hi").await, 200);
    let entry = p.entry("claude-q.json").await;
    let opus = json!({"Anthropic-Ratelimit-Unified-5h-Utilization": "0.40"});
    assert_eq!(entry["quota"]["signals"], opus, "only quota headers are signals");
    assert_eq!(entry["model_quotas"][OPUS]["signals"], opus);

    assert_eq!(p.message(SONNET, "hi").await, 200);
    let entry = p.entry("claude-q.json").await;
    let sonnet = json!({"Anthropic-Ratelimit-Unified-5h-Utilization": "0.41"});
    assert_eq!(entry["quota"]["signals"], sonnet, "the latest response wins");
    assert_eq!(entry["model_quotas"][OPUS]["signals"], opus);
    assert_eq!(entry["model_quotas"][SONNET]["signals"], sonnet);
    let sonnet_seen = entry["model_quotas"][SONNET]["observed_at"].clone();

    // A credential-scope quota failure on Opus refreshes the credential's and Opus's
    // snapshots; Sonnet's stays as observed, without the Retry-After it never saw.
    assert_eq!(p.message(OPUS, "MODE_429").await, 429);
    let rejected = json!({
        "Retry-After": "120",
        "Anthropic-Ratelimit-Unified-5h-Status": "rejected",
        "Anthropic-Ratelimit-Unified-5h-Utilization": "0.99",
    });
    let entry = p.entry("claude-q.json").await;
    assert_eq!(entry["quota"]["signals"], rejected);
    assert_eq!(entry["model_quotas"][OPUS]["signals"], rejected);
    assert_eq!(entry["model_quotas"][SONNET]["signals"], sonnet);
    assert_eq!(entry["model_quotas"][SONNET]["observed_at"], sonnet_seen);

    // The cooldown reaches the sibling (Go's credential_quota): Sonnet is not sent
    // upstream, and cooling it writes no observation.
    assert_ne!(p.message(SONNET, "hi").await, 200);
    assert_eq!(p.upstream_calls(SONNET), 1);
    let cooled = p.entry("claude-q.json").await;
    assert_eq!(cooled["quota"], entry["quota"]);
    assert_eq!(cooled["model_quotas"], entry["model_quotas"]);
}

/// A cooldown backend holding records to restore (Go `CooldownStateStore`).
struct Restore(Vec<Record>);

impl Backend for Restore {
    fn load(&self) -> Result<Vec<Record>, String> {
        Ok(self.0.clone())
    }

    fn save(&self, _: Vec<Record>, _: SystemTime) -> Result<(), String> {
        Ok(())
    }
}

/// TestRestoreCooldownRecordDoesNotOverwriteNewerObservation: restoring a persisted
/// cooldown, even one carrying an older observation time, cools the credential and
/// leaves its live snapshot alone.
#[tokio::test]
async fn restoring_a_cooldown_keeps_the_live_observation() {
    let p = proxy("restore").await;
    assert_eq!(p.message(OPUS, "hi").await, 200);
    let live = p.entry("claude-q.json").await;
    let recover = SystemTime::now() + Duration::from_secs(3600);
    let record = Record {
        provider: "claude".into(),
        auth_id: "claude-q.json".into(),
        status: "cooling".into(),
        next_retry_after: Some(recover),
        reason: "quota".into(),
        quota: Quota {
            exceeded: true,
            reason: "quota".into(),
            next_recover_at: Some(recover),
            observed_at: Some(SystemTime::UNIX_EPOCH + Duration::from_secs(5)),
            ..Default::default()
        },
        updated_at: Some(SystemTime::now()),
        ..Default::default()
    };
    p.rt.set_cooldown_backend(Arc::new(Restore(vec![record])));
    p.rt.store().configure_cooldown_store(Some(p._dir.join("cooldowns")));
    assert_ne!(p.message(OPUS, "hi").await, 200, "the cooldown was restored");
    assert_eq!(p.upstream_calls(OPUS), 1);
    let entry = p.entry("claude-q.json").await;
    assert_eq!(entry["quota"], live["quota"]);
    assert_eq!(entry["model_quotas"], live["model_quotas"]);
}

/// TestProviderSupportsQuotaObservation and
/// TestQuotaStateObserveResponseHeadersDropsKimiGrokAndAntigravitySignals: Claude,
/// Codex and Devin report observations; every other provider reports empty signals.
#[tokio::test]
async fn only_observing_providers_report_snapshots() {
    let p = proxy("providers").await;
    let mut signals = std::collections::BTreeMap::new();
    signals.insert("plan".to_owned(), "pro".to_owned());
    p.rt.executors.devices.devin.observe_quota("devin-d.json", signals);
    let devin = p.entry("devin-d.json").await;
    assert_eq!(devin["quota"]["signals"], json!({"plan": "pro"}));
    assert!(devin["quota"]["observed_at"].is_string(), "{devin}");
    let kimi = p.entry("kimi-k.json").await;
    assert_eq!(kimi["quota"], json!({"signals": {}}));
    assert!(kimi.get("model_quotas").is_none());
}
