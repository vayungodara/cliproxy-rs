//! Bounded trust for quota resets (`routing.cooldown.max-trusted-cooldown`, a
//! cliproxy-rs addition): one Claude credential answers 429 with a weekly reset six
//! days away, then recovers early. The stated reset is trusted only up to the bound,
//! so the next ordinary request after the bound reaches the upstream and succeeds.
//! Run with and without `save-cooldown-status`; with it, a restart restores the bounded
//! cooldown, not the six days. Loopback upstream and fake keys only.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use cpa_core::config::Config;
use cpa_core::credential::Credential;
use cpa_exec::Executors;
use cpa_exec::claude::ClaudeExecutor;
use serde_json::json;

const FILE: &str = "claude-q.json";
/// The bound under test; small so the test does not wait an hour.
const BOUND: Duration = Duration::from_secs(3);

/// The first request is refused with a shared weekly window resetting in six days;
/// every later one succeeds (the provider reset early).
async fn upstream(State(calls): State<Arc<AtomicUsize>>) -> Response {
    if calls.fetch_add(1, Ordering::SeqCst) == 0 {
        let reset = SystemTime::now() + Duration::from_secs(6 * 24 * 3600);
        let reset = reset.duration_since(UNIX_EPOCH).unwrap().as_secs().to_string();
        return (
            StatusCode::TOO_MANY_REQUESTS,
            [
                ("content-type", "application/json"),
                ("anthropic-ratelimit-unified-7d-status", "rejected"),
                ("anthropic-ratelimit-unified-7d-reset", reset.as_str()),
            ],
            r#"{"type":"error","error":{"type":"rate_limit_error","message":"weekly limit"}}"#,
        )
            .into_response();
    }
    (
        [("content-type", "application/json")],
        r#"{"id":"msg_1","type":"message","role":"assistant","content":[],"model":"m","stop_reason":"end_turn","usage":{"input_tokens":1,"output_tokens":1}}"#,
    )
        .into_response()
}

async fn serve(app: axum::Router) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    url
}

/// A proxy over the private `auth_dir` with one API-key credential at `upstream_url`.
async fn proxy(auth_dir: &Path, upstream_url: &str, save: bool) -> (String, Arc<cpa_server::Runtime>) {
    let yaml = format!(
        "api-keys: [client-key]\nauth-dir: {dir}\nrouting:\n  cooldown:\n    \
         save-cooldown-status: {save}\n    max-trusted-cooldown: {bound}s\n",
        dir = auth_dir.display(),
        bound = BOUND.as_secs(),
    );
    let mut credential = Credential::from_file(
        auth_dir,
        &auth_dir.join(FILE),
        json!({"type": "claude"}).as_object().unwrap().clone(),
    )
    .unwrap();
    credential.attributes.insert("api_key".into(), "sk-fake-claude".into());
    credential.attributes.insert("base_url".into(), upstream_url.into());
    let rt = Arc::new(cpa_server::testing::runtime(
        Config::parse(&yaml).unwrap(),
        vec![credential],
        Executors {
            claude: ClaudeExecutor::new("http://127.0.0.1:1").unwrap(),
            codex: Default::default(),
            devices: Default::default(),
            openai: Default::default(),
            google: Default::default(),
        },
    ));
    let app = cpa_server::app(rt.clone(), axum::Router::new());
    (serve(app).await, rt)
}

async fn message(url: &str) -> u16 {
    let body = json!({"model": "claude-opus-4-6", "max_tokens": 8, "messages": [{"role": "user", "content": "hi"}]});
    wreq::Client::new()
        .post(format!("{url}/v1/messages"))
        .header("x-api-key", "client-key")
        .header("content-type", "application/json")
        .body(body.to_string())
        .send()
        .await
        .unwrap()
        .status()
        .as_u16()
}

fn auth_dir(name: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("quota-trust-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

async fn early_reset_is_noticed_after_the_bound(save: bool) {
    let dir = auth_dir(if save { "saved" } else { "memory" });
    let calls = Arc::new(AtomicUsize::new(0));
    let upstream_url = serve(axum::Router::new().fallback(upstream).with_state(calls.clone())).await;
    let (url, rt) = proxy(&dir, &upstream_url, save).await;

    assert_eq!(message(&url).await, 429);
    let cooling = rt.store().cooldowns(FILE);
    assert!(!cooling.is_empty(), "the 429 cools the credential");
    assert_eq!(message(&url).await, 429, "still cooling inside the bound");
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "a cooling credential is not sent upstream"
    );

    let cds = dir.join("claude-q.cds");
    let url = if save {
        assert!(cds.exists(), "save-cooldown-status writes the cooldown");
        let saved = cpa_server::cooldown_store::load(&dir).unwrap();
        let next = saved[0].next_retry_after.unwrap();
        let left = next.duration_since(SystemTime::now()).unwrap_or_default();
        assert!(left <= BOUND, "the file holds the bound, not the six days: {left:?}");
        // A restart restores the bounded cooldown.
        drop(rt);
        let (url, restarted) = proxy(&dir, &upstream_url, save).await;
        assert!(
            !restarted.store().cooldowns(FILE).is_empty(),
            "restored from {}",
            cds.display()
        );
        url
    } else {
        assert!(!cds.exists());
        url
    };

    tokio::time::sleep(BOUND + Duration::from_millis(200)).await;
    assert_eq!(
        message(&url).await,
        200,
        "the first request after the bound is the probe"
    );
    assert_eq!(
        calls.load(Ordering::SeqCst),
        2,
        "exactly one probe reached the upstream"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn early_reset_is_noticed_after_the_bound_in_memory() {
    early_reset_is_noticed_after_the_bound(false).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn early_reset_is_noticed_after_the_bound_with_save_cooldown_status() {
    early_reset_is_noticed_after_the_bound(true).await;
}
