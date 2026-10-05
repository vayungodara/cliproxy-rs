//! Bounded trust for quota resets (`routing.cooldown.max-trusted-cooldown`, a
//! cliproxy-rs addition): one Claude credential answers 429 with a weekly reset six
//! days away, then recovers early. The stated reset is trusted only up to the bound,
//! so the next ordinary request after the bound reaches the upstream and succeeds.
//! Run with and without `save-cooldown-status`; with it, a restart restores the bounded
//! cooldown, not the six days. Requests that arrive together after the bound send one
//! probe upstream. Loopback upstream and fake keys only.

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
/// The bound under test: the smallest allowed (Go's 10 s floor), so the test does not
/// wait an hour.
const BOUND: Duration = Duration::from_secs(10);
const SIX_DAYS: Duration = Duration::from_secs(6 * 24 * 3600);

/// The first request is refused with a shared weekly window resetting in six days;
/// every later one succeeds (the provider reset early), after a short delay so that
/// concurrent requests overlap the probe.
async fn upstream(State(calls): State<Arc<AtomicUsize>>) -> Response {
    if calls.fetch_add(1, Ordering::SeqCst) == 0 {
        let reset = SystemTime::now() + SIX_DAYS;
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
    tokio::time::sleep(Duration::from_millis(300)).await;
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
    proxy_with(auth_dir, upstream_url, save, &format!("{}s", BOUND.as_secs())).await
}

async fn proxy_with(
    auth_dir: &Path,
    upstream_url: &str,
    save: bool,
    bound: &str,
) -> (String, Arc<cpa_server::Runtime>) {
    let yaml = format!(
        "api-keys: [client-key]\nauth-dir: {dir}\nrouting:\n  cooldown:\n    \
         save-cooldown-status: {save}\n    max-trusted-cooldown: \"{bound}\"\n",
        dir = auth_dir.display(),
    );
    // The file exists in `auth-dir`, so the runtime's own reconcile keeps it.
    let meta = json!({"type": "claude", "api_key": "sk-fake-claude", "base_url": upstream_url});
    std::fs::write(auth_dir.join(FILE), meta.to_string()).unwrap();
    let mut credential =
        Credential::from_file(auth_dir, &auth_dir.join(FILE), meta.as_object().unwrap().clone()).unwrap();
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
    let window = cooling
        .iter()
        .map(|c| c.remaining)
        .max()
        .expect("the 429 cools the credential");
    // The six-day header was read: the window is the bound (not Go's 1 s backoff for a
    // 429 without a reset), and the stated reset is kept.
    assert!(window > BOUND / 2 && window <= BOUND, "{window:?}");
    let stated = cooling
        .iter()
        .filter_map(|c| c.recover_in)
        .max()
        .expect("stated reset kept");
    assert!(stated > SIX_DAYS - Duration::from_secs(60), "{stated:?}");
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
        let recover = saved[0].quota.next_recover_at.unwrap();
        assert!(
            recover > SystemTime::now() + SIX_DAYS - Duration::from_secs(60),
            "Go's recovery time"
        );
        assert_eq!(saved[0].quota.trust_level, Some(0));
        // A restart restores the bounded cooldown, neither escalated nor six days.
        drop(rt);
        let (url, restarted) = proxy(&dir, &upstream_url, save).await;
        let restored = restarted.store().cooldowns(FILE);
        let left = restored.iter().map(|c| c.remaining).max().expect("restored");
        assert!(
            left > Duration::ZERO && left <= BOUND,
            "restored from {}: {left:?}",
            cds.display()
        );
        url
    } else {
        assert!(!cds.exists());
        url
    };

    tokio::time::sleep(BOUND + Duration::from_millis(200)).await;
    // Four requests arrive together: the first pick is the probe, the others see the
    // credential cooling until it answers.
    let statuses = futures_util::future::join_all((0..4).map(|_| message(&url))).await;
    assert_eq!(
        statuses.iter().filter(|s| **s == 200).count(),
        1,
        "the probe succeeds: {statuses:?}"
    );
    assert_eq!(
        calls.load(Ordering::SeqCst),
        2,
        "exactly one probe reached the upstream: {statuses:?}"
    );
    assert_eq!(message(&url).await, 200, "the account is back after the probe");
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

/// A bounded `.cds` record with a saved trust count of 2 (the third window) restores
/// at that window's bound, 4 h under the default 1 h, not at the days it names, and
/// keeps its stated reset.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_saved_trust_count_restores_its_window() {
    use cpa_server::cooldown_store::{Quota, Record};
    let dir = auth_dir("count");
    let at = SystemTime::now() + SIX_DAYS;
    let record = Record {
        provider: "claude".into(),
        auth_id: FILE.into(),
        status: "cooling".into(),
        // A bounded record (probe before the stated reset) whose probe time is past its
        // window, as an edited or foreign file would be.
        next_retry_after: Some(at - Duration::from_secs(24 * 3600)),
        reason: "credential_quota".into(),
        quota: Quota {
            exceeded: true,
            reason: "credential_quota".into(),
            next_recover_at: Some(at),
            trust_level: Some(2),
            ..Default::default()
        },
        auth_file: Some(dir.join(FILE)),
        ..Default::default()
    };
    cpa_server::cooldown_store::save(&dir, vec![record], SystemTime::now()).unwrap();
    let (_url, rt) = proxy_with(&dir, "http://127.0.0.1:9", true, "").await;
    let restored = rt.store().cooldowns(FILE);
    let left = restored.iter().map(|c| c.remaining).max().expect("restored");
    let four_hours = Duration::from_secs(4 * 3600);
    assert!(
        left <= four_hours && left > four_hours - Duration::from_secs(60),
        "{left:?}"
    );
    let stated = restored
        .iter()
        .filter_map(|c| c.recover_in)
        .max()
        .expect("stated reset kept");
    assert!(stated > SIX_DAYS - Duration::from_secs(60), "{stated:?}");
    let _ = std::fs::remove_dir_all(&dir);
}
