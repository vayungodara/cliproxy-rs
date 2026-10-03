//! Local-only route checks; no provider endpoints or real credential material.

use std::path::Path;
use std::sync::{Arc, Mutex};

use axum::body::{Body, Bytes};
use axum::extract::{Request, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use cpa_core::config::Config;
use cpa_core::credential::Credential;
use cpa_exec::Executors;
use cpa_exec::claude::ClaudeExecutor;
use cpa_server::runtime::AttemptStats;
use cpa_server::scheduler::Policy;
use cpa_server::{Runtime, router};
use serde_json::Value;

#[derive(Clone, Copy)]
enum Mode {
    FirstFails,
    Quota,
    RequestFault,
    Transport,
    BeforeCommit,
    AfterCommit,
}

struct Mock {
    mode: Mode,
    seen: Mutex<Vec<String>>,
    first_chunk: tokio::sync::Notify,
    break_stream: tokio::sync::Notify,
}

async fn upstream(State(mock): State<Arc<Mock>>, req: Request) -> Response {
    let token = req.headers()["authorization"].to_str().unwrap().to_owned();
    mock.seen.lock().unwrap().push(token.clone());
    match mock.mode {
        Mode::Quota => (StatusCode::TOO_MANY_REQUESTS, [("retry-after", "1")], "quota").into_response(),
        Mode::FirstFails if token == "Bearer fake-a" => {
            (StatusCode::SERVICE_UNAVAILABLE, "first failed").into_response()
        }
        Mode::RequestFault if token == "Bearer fake-a" => (StatusCode::BAD_REQUEST, "payload problem").into_response(),
        Mode::Transport if token == "Bearer fake-a" => {
            let broken = futures_util::stream::iter([Err::<Bytes, _>(std::io::Error::other("mock connection reset"))]);
            ([("content-type", "application/json")], Body::from_stream(broken)).into_response()
        }
        Mode::BeforeCommit if token == "Bearer fake-a" => {
            use futures_util::StreamExt;
            // Headers but no body bytes before the reset. Go's line scanner flushes even
            // a partial line as a chunk, and its Claude handler commits any non-empty
            // chunk, so only a fault before the first byte is a pre-commit fault.
            let first = mock.clone();
            let broken = futures_util::stream::once(async move {
                first.first_chunk.notify_one();
                Ok::<_, std::io::Error>(Bytes::new())
            })
            .chain(futures_util::stream::once(async move {
                mock.break_stream.notified().await;
                Err(std::io::Error::other("mock bootstrap reset"))
            }));
            ([("content-type", "text/event-stream")], Body::from_stream(broken)).into_response()
        }
        Mode::AfterCommit if token == "Bearer fake-a" => {
            use futures_util::StreamExt;
            let broken = futures_util::stream::once(async {
                Ok::<_, std::io::Error>(Bytes::from_static(b"data: committed\n\n"))
            })
            .chain(futures_util::stream::once(async move {
                mock.break_stream.notified().await;
                Err(std::io::Error::other("mock postcommit reset"))
            }));
            ([("content-type", "text/event-stream")], Body::from_stream(broken)).into_response()
        }
        Mode::BeforeCommit | Mode::AfterCommit => {
            ([("content-type", "text/event-stream")], "data: recovered\n\n").into_response()
        }
        _ => ([("content-type", "application/json")], "{\"ok\":true}").into_response(),
    }
}

async fn serve(app: axum::Router) -> (String, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    (
        url,
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() }),
    )
}

struct Fixture {
    url: String,
    rt: Arc<Runtime>,
    mock: Arc<Mock>,
    servers: Vec<tokio::task::JoinHandle<()>>,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        for server in &self.servers {
            server.abort();
        }
    }
}

impl Fixture {
    async fn new(mode: Mode, overrides: &[Value], policy: Policy) -> Self {
        let mock = Arc::new(Mock {
            mode,
            seen: Mutex::default(),
            first_chunk: tokio::sync::Notify::new(),
            break_stream: tokio::sync::Notify::new(),
        });
        let (upstream_url, upstream) = serve(axum::Router::new().fallback(upstream).with_state(mock.clone())).await;
        let credentials = overrides
            .iter()
            .enumerate()
            .map(|(i, extra)| {
                let id = char::from(b'a' + i as u8);
                let mut metadata = extra.as_object().unwrap().clone();
                metadata.insert("type".into(), "claude".into());
                metadata.insert("access_token".into(), format!("fake-{id}").into());
                // The Claude executor below is built on the mock.
                cpa_server::testing::local(
                    Credential::from_file(Path::new("/mock"), &Path::new("/mock").join(id.to_string()), metadata)
                        .unwrap(),
                )
            })
            .collect();
        let rt = Arc::new(cpa_server::testing::runtime(
            Config::parse("").unwrap(),
            credentials,
            Executors {
                claude: ClaudeExecutor::new(upstream_url).unwrap(),
                codex: Default::default(),
                devices: Default::default(),
                openai: Default::default(),
                google: Default::default(),
            },
        ));
        rt.publish_policy(policy);
        let (url, proxy) = serve(router(rt.clone())).await;
        Self {
            url,
            rt,
            mock,
            servers: vec![upstream, proxy],
        }
    }

    async fn request(&self, stream: bool) -> wreq::Response {
        let client = wreq::Client::new();
        let request = client
            .post(format!("{}/v1/messages", self.url))
            .body(format!("{{\"model\":\"claude-sonnet-5\",\"stream\":{stream}}}"))
            .send();
        if matches!(self.mock.mode, Mode::BeforeCommit) {
            let release = async {
                self.mock.first_chunk.notified().await;
                self.mock.break_stream.notify_one();
            };
            let (response, ()) = tokio::time::timeout(std::time::Duration::from_secs(5), async {
                tokio::join!(request, release)
            })
            .await
            .expect("bootstrap request stalled");
            response.unwrap()
        } else {
            request.await.unwrap()
        }
    }

    fn seen(&self) -> Vec<String> {
        self.mock.seen.lock().unwrap().clone()
    }
}

#[tokio::test]
async fn same_round_failover_preserves_lease_counts_and_last_error() {
    let fixture = Fixture::new(
        Mode::FirstFails,
        &[serde_json::json!({}), serde_json::json!({})],
        Policy::default(),
    )
    .await;
    let response = fixture.request(false).await;
    assert_eq!(response.status().as_u16(), 200);
    assert_eq!(response.text().await.unwrap(), "{\"ok\":true}");
    assert_eq!(fixture.seen(), ["Bearer fake-a", "Bearer fake-b"]);
    assert_eq!(
        fixture.rt.store().stats(),
        AttemptStats {
            success: 1,
            failure: 1,
            cancelled: 0
        }
    );

    let fixture = Fixture::new(
        Mode::Quota,
        &[serde_json::json!({}), serde_json::json!({})],
        Policy::default(),
    )
    .await;
    let response = fixture.request(false).await;
    assert_eq!(response.status().as_u16(), 429);
    assert!(
        response.headers().get("retry-after").is_none(),
        "upstream hint is not downstream permission"
    );
    assert!(
        response.text().await.unwrap().contains("quota"),
        "preserve last upstream error"
    );
    assert_eq!(fixture.seen().len(), 2);
    let response = fixture.request(false).await;
    assert_eq!(response.status().as_u16(), 429);
    // The Claude executor adds Go's 1-30s reset fuzz to the 1s upstream hint
    // (claude_ratelimit.go), so the cooldown is max(10s floor, 2..=31s), ceiled.
    let retry_after: u64 = response.headers()["retry-after"].to_str().unwrap().parse().unwrap();
    assert!(
        (10..=31).contains(&retry_after),
        "floor plus ceil only for scheduler error, got {retry_after}"
    );
    assert!(
        response
            .text()
            .await
            .unwrap()
            .contains("All credentials for model claude-sonnet-5 are cooling down")
    );
    assert_eq!(fixture.seen().len(), 2, "cooling credentials never reach upstream");
}

#[tokio::test]
async fn cap_is_per_round_and_skipped_credentials_age_by_round() {
    let policy = Policy {
        max_retry_credentials: 1,
        ..Default::default()
    };
    let fixture = Fixture::new(
        Mode::FirstFails,
        &[
            serde_json::json!({"request_retry":0,"disable_cooling":true}),
            serde_json::json!({"request_retry":0}),
            serde_json::json!({"request_retry":1}),
        ],
        policy,
    )
    .await;
    let response = fixture.request(false).await;
    assert_eq!(response.status().as_u16(), 200);
    assert_eq!(
        fixture.seen(),
        ["Bearer fake-a", "Bearer fake-c"],
        "b cannot join round 1 despite never being tried"
    );

    let fixture = Fixture::new(
        Mode::FirstFails,
        &[serde_json::json!({}), serde_json::json!({})],
        Policy {
            max_retry_credentials: 1,
            request_retry: 1,
            ..Default::default()
        },
    )
    .await;
    assert_eq!(
        fixture.request(false).await.status().as_u16(),
        200,
        "zero max wait allows immediate round with untried b"
    );
    assert_eq!(fixture.seen(), ["Bearer fake-a", "Bearer fake-b"]);

    let fixture = Fixture::new(
        Mode::FirstFails,
        &[serde_json::json!({})],
        Policy {
            request_retry: 3,
            ..Default::default()
        },
    )
    .await;
    assert_eq!(fixture.request(false).await.status().as_u16(), 503);
    assert_eq!(
        fixture.seen().len(),
        1,
        "zero max wait prohibits positive cooldown wait"
    );
}

#[tokio::test]
async fn stop_continue_and_force_cooldown_are_independent() {
    let rule = |action| serde_json::json!({"request_scoped_errors":[{"status":400,"match":["payload"],"action":action}],"disable_cooling":true});
    for (action, expected_status, expected_attempts) in [
        ("stop", 400, 1),
        ("continue", 200, 2),
        ("stop-and-cooldown", 400, 1),
        ("continue-and-cooldown", 200, 2),
    ] {
        let fixture = Fixture::new(
            Mode::RequestFault,
            &[rule(action), serde_json::json!({})],
            Policy::default(),
        )
        .await;
        assert_eq!(
            fixture.request(false).await.status().as_u16(),
            expected_status,
            "{action}"
        );
        assert_eq!(fixture.seen().len(), expected_attempts, "{action}");
        let selection = cpa_server::runtime::Selection {
            provider: "claude".into(),
            model: "claude-sonnet-5".into(),
            exclude: vec!["b".into()],
            ..Default::default()
        };
        assert_eq!(
            fixture.rt.store().select(selection).is_none(),
            action.ends_with("and-cooldown"),
            "{action}"
        );
    }
}

#[tokio::test]
async fn transport_and_precommit_faults_fail_over_without_poisoning() {
    for mode in [Mode::Transport, Mode::BeforeCommit] {
        let fixture = Fixture::new(mode, &[serde_json::json!({}), serde_json::json!({})], Policy::default()).await;
        for _ in 0..2 {
            let response = fixture.request(matches!(mode, Mode::BeforeCommit)).await;
            assert_eq!(response.status().as_u16(), 200);
            let text = response.text().await.unwrap();
            assert!(!text.contains("partial"));
        }
        assert_eq!(
            fixture.seen(),
            ["Bearer fake-a", "Bearer fake-b", "Bearer fake-a", "Bearer fake-b"]
        );
        assert_eq!(
            fixture.rt.store().stats(),
            AttemptStats {
                success: 2,
                failure: 2,
                cancelled: 0
            }
        );
    }
}

#[tokio::test]
async fn postcommit_stream_fault_is_terminal_and_never_replayed() {
    let fixture = Fixture::new(
        Mode::AfterCommit,
        &[serde_json::json!({}), serde_json::json!({})],
        Policy {
            request_retry: 3,
            ..Default::default()
        },
    )
    .await;
    let response = fixture.request(true).await;
    assert_eq!(response.status().as_u16(), 200);
    // Headers prove the first event committed; only then cut the upstream stream.
    fixture.mock.break_stream.notify_one();
    let text = response.text().await.unwrap();
    assert!(text.starts_with("data: committed\n\n"));
    assert!(text.contains("event: error\n"));
    assert!(!text.contains("recovered"));
    assert_eq!(fixture.seen(), ["Bearer fake-a"]);
    assert_eq!(
        fixture.rt.store().stats(),
        AttemptStats {
            success: 0,
            failure: 1,
            cancelled: 0
        }
    );
}
