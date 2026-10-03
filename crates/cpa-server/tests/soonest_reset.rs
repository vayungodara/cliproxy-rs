//! `routing.strategy: soonest-reset` (a cliproxy-rs addition) through the router, the
//! Claude executor's quota observations and the management API, against a loopback
//! upstream that reports a different weekly reset per account. Fake keys only.

use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use axum::extract::{Request, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use cpa_core::config::Config;
use cpa_exec::Executors;
use cpa_exec::claude::ClaudeExecutor;
use cpa_server::management::{Management, Options};
use cpa_server::scheduler::Strategy;
use serde_json::{Value, json};

const PASSWORD: &str = "fake-management-password";
const DAY: u64 = 24 * 3600;

/// What each account's next answer says, by API key.
#[derive(Default)]
struct Upstream {
    /// API keys of the Messages requests, in order.
    keys: Mutex<Vec<String>>,
    /// The account whose weekly window resets in one day (the others: four).
    soon: Mutex<String>,
    /// An account whose 5-hour window is used up.
    five_hour_out: Mutex<String>,
    /// An account that answers 429 (its quota ran out).
    exhausted: Mutex<String>,
    /// An account whose answers carry quota signals but no weekly reset.
    no_reset: Mutex<String>,
}

fn epoch(after: u64) -> String {
    (SystemTime::now() + Duration::from_secs(after))
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap()
        .as_secs()
        .to_string()
}

async fn upstream(State(up): State<Arc<Upstream>>, req: Request) -> Response {
    // The account key arrives as x-api-key or a bearer token, by origin.
    let header = |name: &str| req.headers().get(name).map(|v| v.to_str().unwrap().to_owned());
    let key = header("x-api-key")
        .or_else(|| header("authorization").map(|v| v.trim_start_matches("Bearer ").to_owned()))
        .unwrap_or_default();
    up.keys.lock().unwrap().push(key.clone());
    let weekly = epoch(if *up.soon.lock().unwrap() == key { DAY } else { 4 * DAY });
    if *up.exhausted.lock().unwrap() == key {
        return (
            StatusCode::TOO_MANY_REQUESTS,
            [
                ("content-type", "application/json".to_owned()),
                ("anthropic-ratelimit-unified-7d-status", "rejected".to_owned()),
                ("anthropic-ratelimit-unified-7d-reset", weekly),
                ("retry-after", "3600".to_owned()),
            ],
            r#"{"type":"error","error":{"type":"rate_limit_error","message":"quota"}}"#,
        )
            .into_response();
    }
    let five_hour = if *up.five_hour_out.lock().unwrap() == key {
        "rejected"
    } else {
        "allowed"
    };
    if *up.no_reset.lock().unwrap() == key {
        return (
            [
                ("content-type", "application/json".to_owned()),
                ("anthropic-ratelimit-unified-5h-status", five_hour.to_owned()),
            ],
            r#"{"id":"msg_1","type":"message","role":"assistant","content":[],"model":"m","stop_reason":"end_turn","usage":{"input_tokens":1,"output_tokens":1}}"#,
        )
            .into_response();
    }
    (
        [
            ("content-type", "application/json".to_owned()),
            ("anthropic-ratelimit-unified-7d-status", "allowed".to_owned()),
            ("anthropic-ratelimit-unified-7d-reset", weekly),
            ("anthropic-ratelimit-unified-5h-status", five_hour.to_owned()),
            ("anthropic-ratelimit-unified-5h-reset", epoch(3600)),
        ],
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

struct Proxy {
    url: String,
    rt: Arc<cpa_server::Runtime>,
    up: Arc<Upstream>,
    base: String,
    /// The two account keys, `x` first in credential ID order (where equal ranks
    /// start), `y` second.
    x: String,
    y: String,
    _dir: std::path::PathBuf,
}

/// A private `auth-dir`: without one, credential loading reads the user's real
/// `~/.cli-proxy-api`.
fn yaml(base: &str, strategy: &str, auth_dir: &std::path::Path) -> String {
    format!(
        "auth-dir: {}\napi-keys:\n  - client-key\nrouting:\n  strategy: {strategy}\nclaude-api-key:\n  - api-key: sk-fake-1\n    base-url: {base}\n  - api-key: sk-fake-2\n    base-url: {base}\n",
        auth_dir.display()
    )
}

async fn proxy(name: &str, strategy: &str) -> Proxy {
    let up = Arc::new(Upstream::default());
    let base = serve(axum::Router::new().fallback(upstream).with_state(up.clone())).await;
    let dir = std::env::temp_dir().join(format!("cpa-soonest-{name}-{}", std::process::id()));
    let auth_dir = dir.join("auths");
    std::fs::create_dir_all(&auth_dir).unwrap();
    let yaml = yaml(&base, strategy, &auth_dir);
    let config = Config::parse(&yaml).unwrap();
    let mut credentials = cpa_core::config::credentials::load(&config);
    assert_eq!(credentials.len(), 2, "only the two config keys");
    credentials.sort_by(|a, b| a.id.cmp(&b.id));
    let key = |i: usize| credentials[i].attributes["api_key"].clone();
    let (x, y) = (key(0), key(1));
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
    let path = dir.join("config.yaml");
    std::fs::write(&path, &yaml).unwrap();
    let options = Options {
        management_password: Some(PASSWORD.into()),
        ..Default::default()
    };
    let management = Management::with_options(rt.clone(), path, options);
    let app = cpa_server::app(rt.clone(), cpa_server::management::router(management));
    Proxy {
        url: serve(app).await,
        rt,
        up,
        base,
        x,
        y,
        _dir: dir,
    }
}

impl Proxy {
    async fn message(&self) -> (u16, String) {
        let res = wreq::Client::new()
            .post(format!("{}/v1/messages", self.url))
            .header("x-api-key", "client-key")
            .header("content-type", "application/json")
            .body(r#"{"model":"claude-sonnet-4-6","max_tokens":8,"messages":[{"role":"user","content":"hi"}]}"#)
            .send()
            .await
            .unwrap();
        (res.status().as_u16(), res.text().await.unwrap())
    }

    /// The accounts (`x` or `y`) the next `n` requests reached, failovers included.
    async fn route(&self, n: usize) -> Vec<&'static str> {
        let before = self.up.keys.lock().unwrap().len();
        for _ in 0..n {
            let (status, body) = self.message().await;
            assert_eq!(status, 200, "{body}");
        }
        self.up.keys.lock().unwrap()[before..]
            .iter()
            .map(|k| match k {
                k if *k == self.x => "x",
                k if *k == self.y => "y",
                other => panic!("unexpected upstream key {other:?}"),
            })
            .collect()
    }

    async fn management(&self, method: &str, path: &str, body: Option<Value>) -> (u16, Value) {
        let client = wreq::Client::new();
        let url = format!("{}{path}", self.url);
        let req = match method {
            "PUT" => client.put(url),
            _ => client.get(url),
        }
        .bearer_auth(PASSWORD);
        let req = match body {
            Some(body) => req.json(&body),
            None => req,
        };
        let res = req.send().await.unwrap();
        let status = res.status().as_u16();
        (status, res.json().await.unwrap_or(Value::Null))
    }
}

#[tokio::test]
async fn soonest_weekly_reset_is_used_up_first_then_the_next() {
    let p = proxy("route", "soonest-reset").await;
    assert_eq!(p.rt.policy().strategy, Strategy::SoonestReset);
    // `y` resets in one day, `x` in four; nothing is observed yet. Each unknown account
    // gets one probe request (equal ranks start at `x`), then the sooner reset takes
    // every request.
    *p.up.soon.lock().unwrap() = p.y.clone();
    assert_eq!(p.route(5).await, ["x", "y", "y", "y", "y"]);
    // `y` reports its 5-hour window used up: `x` takes over.
    *p.up.five_hour_out.lock().unwrap() = p.y.clone();
    assert_eq!(p.route(1).await, ["y"]);
    assert_eq!(p.route(2).await, ["x", "x"]);
    // `x` runs out: its 429 fails over to `y`, the only account left, and cools `x`.
    *p.up.exhausted.lock().unwrap() = p.x.clone();
    assert_eq!(p.route(1).await, ["x", "y"]);
    assert_eq!(p.route(2).await, ["y", "y"]);
}

/// An account whose answers carry quota signals but never a weekly reset gets one
/// probe; its later answers (each a newer observation) do not re-arm it.
#[tokio::test]
async fn answers_without_a_reset_do_not_re_arm_the_probe() {
    let p = proxy("noreset", "soonest-reset").await;
    *p.up.soon.lock().unwrap() = p.y.clone();
    *p.up.no_reset.lock().unwrap() = p.x.clone();
    assert_eq!(p.route(5).await, ["x", "y", "y", "y", "y"]);
}

/// A probed account that resets later than the known one gets its one request, then
/// the traffic goes back.
#[tokio::test]
async fn a_probed_later_reset_hands_back_to_the_sooner_one() {
    let p = proxy("probe", "soonest-reset").await;
    *p.up.soon.lock().unwrap() = p.x.clone();
    assert_eq!(p.route(5).await, ["x", "y", "x", "x", "x"]);
}

#[tokio::test]
async fn management_api_and_reload_switch_the_strategy() {
    let p = proxy("manage", "round-robin").await;
    *p.up.soon.lock().unwrap() = p.y.clone();
    assert_eq!(p.route(4).await, ["x", "y", "x", "y"], "the default spreads evenly");
    // v8 config path, with the alias: the published policy follows at once.
    let (status, body) = p
        .management(
            "PUT",
            "/v8/management/config/routing/strategy",
            Some(json!("reset-first")),
        )
        .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(p.rt.policy().strategy, Strategy::SoonestReset);
    assert_eq!(p.route(3).await, ["y", "y", "y"]);
    // The deprecated v0 route accepts and normalizes it like Go's other values.
    let (status, body) = p
        .management(
            "PUT",
            "/v0/management/routing/strategy",
            Some(json!({"value": "Reset-First"})),
        )
        .await;
    assert_eq!(status, 200, "{body}");
    let (_, got) = p.management("GET", "/v0/management/routing/strategy", None).await;
    assert_eq!(got, json!({"strategy": "soonest-reset"}));
    // A reloaded config (the file watcher's publish) switches back to spreading.
    p.rt.publish_config(Config::parse(&yaml(&p.base, "round-robin", &p._dir.join("auths"))).unwrap());
    assert_eq!(p.rt.policy().strategy, Strategy::RoundRobin);
    let spread = p.route(2).await;
    assert!(spread.contains(&"x") && spread.contains(&"y"), "{spread:?}");
}
