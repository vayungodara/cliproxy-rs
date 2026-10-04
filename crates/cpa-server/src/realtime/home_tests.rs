//! Go live_test.go's Home cases (`TestHandlerUsesLiveModelForHomeDispatch`,
//! `TestHomeLiveSessionExpiryReleasesSelection`,
//! `TestHandlerReleasesHomeSelectionWhenMediaSetupFails`), `SelectHomeAuthByKind`'s
//! redispatch, and the selection's lifetime on the sideband, hangup and direct socket,
//! with a fake Home dispatcher.

use std::path::Path;
use std::sync::Mutex;
use std::time::Duration;

use axum::extract::ws::{Message as WsMessage, WebSocketUpgrade, rejection::WebSocketUpgradeRejection};
use axum::http::{HeaderValue, StatusCode, Uri, header};
use axum::response::IntoResponse;
use cpa_core::credential::Credential;
use cpa_core::exec::{ExecError, FailureScope};
use futures_util::future::BoxFuture;
use tokio::sync::Notify;
use wreq::ws::message::Message;

use super::relay::{CloseHandler, Hold, MediaRelay, MediaSession, NewSession, RelayError, Route};
use super::*;
use crate::remote::{ModelsError, RemoteDispatch, RemoteError, RemoteErrorKind, RemoteGrant, RemoteRequest};

/// Every lease end (the credential ID) and media close ("media_closed"), in order.
type Log = Arc<Mutex<Vec<String>>>;

/// Hands out `grants` in order (the last one repeats) and records every request. With
/// `ack`, a release completes when it is notified, with `ack_result`.
#[derive(Default)]
struct FakeHome {
    grants: Mutex<Vec<Credential>>,
    requests: Mutex<Vec<RemoteRequest>>,
    log: Log,
    fail: Option<RemoteError>,
    ack: Option<Arc<Notify>>,
    ack_result: Option<String>,
}

impl FakeHome {
    fn with(grants: Vec<Credential>) -> Arc<Self> {
        Arc::new(Self {
            grants: Mutex::new(grants),
            ..Self::default()
        })
    }

    fn log(&self) -> Vec<String> {
        self.log.lock().unwrap().clone()
    }

    fn requests(&self) -> Vec<RemoteRequest> {
        self.requests.lock().unwrap().clone()
    }
}

impl RemoteDispatch for FakeHome {
    fn available(&self) -> bool {
        true
    }

    fn dispatch(&self, request: RemoteRequest) -> BoxFuture<'_, Result<RemoteGrant, RemoteError>> {
        Box::pin(async move {
            self.requests.lock().unwrap().push(request);
            if let Some(error) = &self.fail {
                return Err(error.clone());
            }
            let credential = {
                let mut grants = self.grants.lock().unwrap();
                if grants.len() > 1 {
                    grants.remove(0)
                } else {
                    grants[0].clone()
                }
            };
            let (log, id) = (self.log.clone(), credential.id.clone());
            let (ack, result) = (self.ack.clone(), self.ack_result.clone());
            Ok(RemoteGrant {
                credential,
                end: Box::new(move || {
                    log.lock().unwrap().push(id);
                    let ack = ack?;
                    Some(Box::pin(async move {
                        ack.notified().await;
                        result.map_or(Ok(()), Err)
                    }) as crate::remote::ReleaseWait)
                }),
                cancel: None,
                request_retry: None,
                user_api_key: Default::default(),
            })
        })
    }

    fn models(
        &self,
        _: Vec<(String, String)>,
        _: Vec<(String, String)>,
    ) -> BoxFuture<'_, Result<Vec<u8>, ModelsError>> {
        Box::pin(async { Err(ModelsError::Unavailable) })
    }
}

/// A dispatched credential: `kind` "oauth" or "apikey".
fn credential(id: &str, provider: &str, kind: &str) -> Credential {
    let meta = serde_json::json!({"type": provider, "access_token": format!("{id}-token")});
    let mut credential = Credential::from_file(
        Path::new("/fake"),
        Path::new(&format!("/fake/{id}.json")),
        meta.as_object().unwrap().clone(),
    )
    .unwrap();
    credential.id = id.into();
    credential.attributes.insert("auth_kind".into(), kind.into());
    credential
}

fn live_credential() -> Credential {
    credential("home-codex-live", "codex", "oauth")
}

/// What the upstream saw: path and Authorization.
type Seen = Arc<Mutex<Vec<(String, String)>>>;

async fn serve(app: axum::Router) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    format!("http://{addr}")
}

/// The upstream: calls answer 201 with `location` (200 without one), hangups 200, and
/// sockets echo `echo:<text>` until closed. A target containing `fail` gets a 404.
async fn start(
    home: Arc<FakeHome>,
    location: Option<&'static str>,
    lifetime: Duration,
    relay: Option<Arc<dyn MediaRelay>>,
) -> (String, Arc<Live>, Seen) {
    let seen = Seen::default();
    let record = seen.clone();
    let upstream = axum::Router::new().fallback(
        move |uri: Uri, headers: axum::http::HeaderMap, ws: Result<WebSocketUpgrade, WebSocketUpgradeRejection>| {
            let record = record.clone();
            async move {
                let auth = headers
                    .get(header::AUTHORIZATION)
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or_default()
                    .to_owned();
                record.lock().unwrap().push((uri.path().to_owned(), auth));
                if uri.to_string().contains("fail") {
                    return (StatusCode::NOT_FOUND, "no such call").into_response();
                }
                if let Ok(ws) = ws {
                    return ws.on_upgrade(|mut socket| async move {
                        while let Some(Ok(message)) = socket.recv().await {
                            match message {
                                WsMessage::Text(text) => {
                                    let echo = format!("echo:{}", text.as_str());
                                    if socket.send(WsMessage::Text(echo.into())).await.is_err() {
                                        return;
                                    }
                                }
                                WsMessage::Close(_) => return,
                                _ => {}
                            }
                        }
                    });
                }
                if uri.path().ends_with("/hangup") {
                    return (StatusCode::OK, "{}").into_response();
                }
                let status = if location.is_some() {
                    StatusCode::CREATED
                } else {
                    StatusCode::OK
                };
                let mut response = (status, "v=0\r\no=upstream-answer\r\n").into_response();
                response
                    .headers_mut()
                    .insert(header::CONTENT_TYPE, HeaderValue::from_static("application/sdp"));
                if let Some(location) = location {
                    response
                        .headers_mut()
                        .insert(header::LOCATION, HeaderValue::from_static(location));
                }
                response
            }
        },
    );
    let upstream_url = serve(upstream).await;
    let executor = cpa_exec::codex::CodexExecutor::with_client(
        wreq::Client::new(),
        cpa_exec::codex_oauth::CodexOAuth::new(wreq::Client::new()),
    )
    .with_live_endpoints(
        format!("{upstream_url}/calls"),
        format!("ws{}/v1", upstream_url.trim_start_matches("http")),
    );
    let rt = Arc::new(crate::testing::runtime(
        // A private auth-dir: the test never reads the user's.
        cpa_core::config::Config::parse(&format!(
            "auth-dir: '{}'\n",
            std::env::temp_dir()
                .join(format!("cpa-live-home-{}", uuid::Uuid::new_v4()))
                .display()
        ))
        .unwrap(),
        vec![],
        cpa_exec::Executors {
            claude: cpa_exec::claude::ClaudeExecutor::new("http://127.0.0.1:1").unwrap(),
            codex: executor,
            devices: Default::default(),
            openai: Default::default(),
            google: Default::default(),
        },
    ));
    rt.set_remote_dispatch(Some(home as Arc<dyn RemoteDispatch>));
    let mut live = Live::default();
    live.calls = calls::Calls::new_shared(lifetime);
    live.relays.fixed = relay;
    let live = Arc::new(live);
    let app = axum::Router::new().merge(routes_with(&rt, live.clone())).with_state(rt);
    (serve(app).await, live, seen)
}

const BOUNDARY: &str = "home-live-boundary";
const HOUR: Duration = Duration::from_secs(3600);

async fn post_live(proxy: &str, model: &str) -> wreq::Response {
    let body = format!(
        "--{BOUNDARY}\r\nContent-Disposition: form-data; name=\"sdp\"\r\n\r\nv=0\r\n--{BOUNDARY}\r\nContent-Disposition: form-data; name=\"session\"\r\n\r\n{{\"model\":\"{model}\"}}\r\n--{BOUNDARY}--\r\n"
    );
    wreq::Client::new()
        .post(format!("{proxy}/v1/live"))
        .header("content-type", format!("multipart/form-data; boundary={BOUNDARY}"))
        .body(body)
        .send()
        .await
        .unwrap()
}

/// Opens a WebSocket through the proxy and checks one echo.
async fn open_socket(proxy: &str, path: &str) -> wreq::ws::WebSocket {
    let response = wreq::Client::new()
        .websocket(format!("ws{}{path}", proxy.trim_start_matches("http")))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status().as_u16(), 101, "{path}");
    let mut socket = response.into_websocket().await.unwrap();
    socket.send(Message::text("ping")).await.unwrap();
    let echoed = tokio::time::timeout(Duration::from_secs(2), socket.recv()).await;
    assert!(matches!(echoed, Ok(Some(Ok(Message::Text(t)))) if t.as_str() == "echo:ping"));
    socket
}

async fn eventually(what: &str, mut check: impl FnMut() -> bool) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    while !check() {
        assert!(tokio::time::Instant::now() < deadline, "{what}");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

#[tokio::test]
async fn live_model_goes_to_home_and_the_call_holds_its_credential() {
    let home = FakeHome::with(vec![live_credential()]);
    let (proxy, live, seen) = start(home.clone(), Some("/v1/live/call-123"), HOUR, None).await;
    let response = post_live(&proxy, "future-live-model").await;
    assert_eq!(response.status().as_u16(), 201);
    let requests = home.requests();
    assert_eq!(requests.len(), 1);
    let request = &requests[0];
    assert_eq!(
        (
            request.model.as_str(),
            request.kind,
            request.count,
            request.pinned.as_str()
        ),
        ("future-live-model", "http", 1, "")
    );
    assert!(request.excluded.is_empty());
    assert_eq!(seen.lock().unwrap()[0].1, "Bearer home-codex-live-token");
    assert!(home.log().is_empty(), "the stored call holds the Home selection");
    let call = live.calls.peek("call-123").expect("stored call");
    assert_eq!(
        call.home
            .as_ref()
            .and_then(|h| h.active())
            .map(|c| c.id.clone())
            .as_deref(),
        Some("home-codex-live")
    );
    assert_eq!(call.model, "future-live-model");

    // The hangup uses the held credential; ending the call ends the selection.
    let hangup = wreq::Client::new()
        .post(format!("{proxy}/v1/realtime/calls/call-123/hangup"))
        .send()
        .await
        .unwrap();
    assert_eq!(hangup.status().as_u16(), 200);
    assert_eq!(home.requests().len(), 1, "no new Home pick for the hangup");
    let last = seen.lock().unwrap().last().cloned().unwrap();
    assert!(last.0.ends_with("/realtime/calls/call-123/hangup"), "{last:?}");
    assert_eq!(last.1, "Bearer home-codex-live-token");
    assert_eq!(home.log(), ["home-codex-live"]);
    assert!(live.calls.peek("call-123").is_none());
}

#[tokio::test]
async fn call_expiry_releases_the_home_selection() {
    let home = FakeHome::with(vec![live_credential()]);
    let (proxy, live, _) = start(home.clone(), Some("/v1/live/call-123"), Duration::from_millis(30), None).await;
    assert_eq!(post_live(&proxy, "gpt-live-1-codex").await.status().as_u16(), 201);
    eventually("expired call released its Home selection", || {
        live.calls.peek("call-123").is_none() && home.log() == ["home-codex-live"]
    })
    .await;
}

#[tokio::test]
async fn unstored_responses_end_the_selection_with_the_request() {
    let home = FakeHome::with(vec![live_credential()]);
    let (proxy, _, _) = start(home.clone(), None, HOUR, None).await;
    assert_eq!(post_live(&proxy, "gpt-live-1-codex").await.status().as_u16(), 200);
    eventually("request end released the Home selection", || {
        home.log() == ["home-codex-live"]
    })
    .await;
}

/// Close-completion hooks held back until the test runs them.
type Pending = Arc<Mutex<Vec<Box<dyn FnOnce() + Send>>>>;

fn finish(pending: &Pending) {
    let hooks = std::mem::take(&mut *pending.lock().unwrap());
    for hook in hooks {
        hook();
    }
}

/// A relay that fails setup, or hands out sessions that log their close. With
/// `delayed`, a close completes only when the test calls [`finish`]. With `hang`, setup
/// never finishes. Like the real relay, a setup that fails or is dropped closes its
/// half-built session and keeps the hold until that close finished.
struct FakeRelay {
    fail: bool,
    hang: bool,
    log: Log,
    delayed: Option<Pending>,
}

/// A half-built session: dropped while it has the hold, it closes and releases the hold
/// after the close.
struct FakeSetup {
    log: Log,
    delayed: Option<Pending>,
    hold: Option<Hold>,
}

impl Drop for FakeSetup {
    fn drop(&mut self) {
        let Some(hold) = self.hold.take() else { return };
        self.log.lock().unwrap().push("media_closed".into());
        match &self.delayed {
            Some(pending) => pending.lock().unwrap().push(Box::new(move || drop(hold))),
            None => drop(hold),
        }
    }
}

struct FakeSession {
    log: Log,
    delayed: Option<Pending>,
}

impl MediaSession for FakeSession {
    fn accept_upstream_answer(&self, _: String) -> BoxFuture<'_, Result<String, RelayError>> {
        Box::pin(async { Ok("v=0\r\no=downstream-answer\r\n".to_owned()) })
    }
    fn set_call_id(&self, _: &str) {}
    fn set_close_handler(&self, _: CloseHandler) {}
    fn close(&self, _: &str) {
        self.log.lock().unwrap().push("media_closed".into());
    }
    fn after_close(&self, then: Box<dyn FnOnce() + Send>) {
        match &self.delayed {
            Some(pending) => pending.lock().unwrap().push(then),
            None => then(),
        }
    }
}

impl MediaRelay for FakeRelay {
    fn new_session(&self, _: String, _: Route, hold: Hold) -> BoxFuture<'_, NewSession> {
        Box::pin(async move {
            let setup = FakeSetup {
                log: self.log.clone(),
                delayed: self.delayed.clone(),
                hold: Some(hold),
            };
            if self.hang {
                std::future::pending::<()>().await;
            }
            if self.fail {
                drop(setup);
                // The error returns once the close finished.
                if let Some(pending) = &self.delayed {
                    let (done, closed) = tokio::sync::oneshot::channel::<()>();
                    pending.lock().unwrap().push(Box::new(move || drop(done)));
                    let _ = closed.await;
                }
                return Err(RelayError::new("media setup failed"));
            }
            let mut setup = setup;
            // Started: the hold goes back and nothing closes.
            drop(setup.hold.take());
            let session = FakeSession {
                log: self.log.clone(),
                delayed: self.delayed.clone(),
            };
            Ok((
                Arc::new(session) as Arc<dyn MediaSession>,
                "v=0\r\no=gateway-offer\r\n".to_owned(),
            ))
        })
    }
}

#[tokio::test]
async fn media_setup_failure_releases_the_home_selection() {
    let home = FakeHome::with(vec![live_credential()]);
    let relay: Arc<dyn MediaRelay> = Arc::new(FakeRelay {
        fail: true,
        hang: false,
        log: home.log.clone(),
        delayed: None,
    });
    let (proxy, _, seen) = start(home.clone(), Some("/v1/live/call-123"), HOUR, Some(relay)).await;
    let response = post_live(&proxy, "gpt-live-1-codex").await;
    assert_eq!(response.status().as_u16(), 502);
    assert_eq!(response.text().await.unwrap(), r#"{"error":"media setup failed"}"#);
    assert!(seen.lock().unwrap().is_empty(), "nothing went upstream");
    assert_eq!(home.log(), ["media_closed", "home-codex-live"]);
}

/// Go's defers: unretained media closes before the selection ends, and a call's media
/// closes before its selection ends.
#[tokio::test]
async fn media_closes_before_the_home_selection_ends() {
    let home = FakeHome::with(vec![live_credential()]);
    let relay = || -> Arc<dyn MediaRelay> {
        Arc::new(FakeRelay {
            fail: false,
            hang: false,
            log: home.log.clone(),
            delayed: None,
        })
    };
    // 200 without a call ID: the relayed answer is refused and nothing is kept.
    let (proxy, _, _) = start(home.clone(), None, HOUR, Some(relay())).await;
    let response = post_live(&proxy, "gpt-live-1-codex").await;
    assert_eq!(response.status().as_u16(), 502);
    assert_eq!(home.log(), ["media_closed", "home-codex-live"]);

    home.log.lock().unwrap().clear();
    let (proxy, live, _) = start(home.clone(), Some("/v1/live/call-123"), HOUR, Some(relay())).await;
    assert_eq!(post_live(&proxy, "gpt-live-1-codex").await.status().as_u16(), 201);
    assert!(home.log().is_empty());
    live.calls.close_all("test_done");
    assert_eq!(home.log(), ["media_closed", "home-codex-live"]);
}

#[tokio::test]
async fn ineligible_home_picks_are_ended_and_excluded() {
    let home = FakeHome::with(vec![
        credential("home-claude", "claude", "oauth"),
        credential("home-codex-key", "codex", "apikey"),
        live_credential(),
    ]);
    let (proxy, _, seen) = start(home.clone(), Some("/v1/live/call-123"), HOUR, None).await;
    assert_eq!(post_live(&proxy, "gpt-live-1-codex").await.status().as_u16(), 201);
    let requests = home.requests();
    let picks: Vec<(i64, Vec<String>)> = requests.iter().map(|r| (r.count, r.excluded.clone())).collect();
    assert_eq!(
        picks,
        vec![
            (1, vec![]),
            (2, vec!["home-claude".to_owned()]),
            (3, vec!["home-claude".to_owned(), "home-codex-key".to_owned()]),
        ]
    );
    assert!(requests.iter().all(|r| r.request_id == requests[0].request_id));
    assert_eq!(home.log(), ["home-claude", "home-codex-key"]);
    assert_eq!(seen.lock().unwrap()[0].1, "Bearer home-codex-live-token");
}

/// `endHomeSelectionBeforeRedispatch`: the next pick waits for Home's acknowledgement.
#[tokio::test]
async fn redispatch_waits_for_the_release_acknowledgement() {
    let ack = Arc::new(Notify::new());
    let home = Arc::new(FakeHome {
        grants: Mutex::new(vec![credential("home-codex-key", "codex", "apikey"), live_credential()]),
        ack: Some(ack.clone()),
        ..FakeHome::default()
    });
    let (proxy, _, _) = start(home.clone(), Some("/v1/live/call-123"), HOUR, None).await;
    let pending = tokio::spawn(async move { post_live(&proxy, "gpt-live-1-codex").await.status().as_u16() });
    eventually("the ineligible pick ended", || home.log() == ["home-codex-key"]).await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(
        home.requests().len(),
        1,
        "no new pick before Home acknowledged the release"
    );
    ack.notify_one();
    assert_eq!(pending.await.unwrap(), 201);
    assert_eq!(home.requests().len(), 2);
}

#[tokio::test]
async fn an_unacknowledged_release_fails_like_go() {
    let ack = Arc::new(Notify::new());
    ack.notify_one();
    let home = Arc::new(FakeHome {
        grants: Mutex::new(vec![credential("home-codex-key", "codex", "apikey"), live_credential()]),
        ack: Some(ack),
        ack_result: Some("context deadline exceeded".into()),
        ..FakeHome::default()
    });
    let (proxy, _, seen) = start(home.clone(), Some("/v1/live/call-123"), HOUR, None).await;
    let response = post_live(&proxy, "gpt-live-1-codex").await;
    assert_eq!(response.status().as_u16(), 503);
    assert_eq!(
        response.text().await.unwrap(),
        r#"{"error":"home_unavailable: Home did not acknowledge credential release: context deadline exceeded"}"#
    );
    assert_eq!(home.requests().len(), 1);
    assert!(seen.lock().unwrap().is_empty());
}

#[tokio::test]
async fn a_repeated_ineligible_pick_fails_like_go() {
    let home = FakeHome::with(vec![credential("home-codex-key", "codex", "apikey")]);
    let (proxy, _, seen) = start(home.clone(), Some("/v1/live/call-123"), HOUR, None).await;
    let response = post_live(&proxy, "gpt-live-1-codex").await;
    assert_eq!(response.status().as_u16(), 503);
    assert_eq!(
        response.text().await.unwrap(),
        r#"{"error":"auth_not_found: selector repeatedly returned an ineligible auth"}"#
    );
    assert_eq!(home.requests().len(), 2);
    assert_eq!(home.log(), ["home-codex-key", "home-codex-key"]);
    assert!(seen.lock().unwrap().is_empty());
}

#[tokio::test]
async fn home_pick_errors_render_like_write_selection_error() {
    let home = Arc::new(FakeHome {
        fail: Some(RemoteError {
            error: ExecError::local(
                429,
                FailureScope::Credential,
                "model_cooldown: all credentials for model gpt-live-1-codex are cooling down",
            ),
            code: "model_cooldown".into(),
            kind: RemoteErrorKind::Cooldown {
                retry_after: Some(Duration::from_millis(2500)),
                request_retry: None,
            },
        }),
        ..FakeHome::default()
    });
    let (proxy, _, _) = start(home, Some("/v1/live/call-123"), HOUR, None).await;
    let response = post_live(&proxy, "gpt-live-1-codex").await;
    assert_eq!(response.status().as_u16(), 429);
    assert_eq!(response.headers()["retry-after"], "3");
    assert_eq!(
        response.text().await.unwrap(),
        r#"{"error":"model_cooldown: all credentials for model gpt-live-1-codex are cooling down"}"#
    );
}

/// Go `Retain` then `End("session_closed")`: the direct socket holds its selection.
#[tokio::test]
async fn direct_websocket_holds_its_selection_while_open() {
    let home = FakeHome::with(vec![live_credential()]);
    let (proxy, _, seen) = start(home.clone(), None, HOUR, None).await;
    let mut socket = open_socket(&proxy, "/v1/realtime?model=gpt-realtime").await;
    let requests = home.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].kind, "websocket");
    assert_eq!(requests[0].model, cpa_exec::codex_live::codex_model("gpt-realtime"));
    assert_eq!(seen.lock().unwrap()[0].1, "Bearer home-codex-live-token");
    tokio::time::sleep(Duration::from_millis(30)).await;
    assert!(home.log().is_empty(), "held while the socket is open");
    socket.send(Message::close(None)).await.unwrap();
    drop(socket);
    eventually("the closed socket released its selection", || {
        home.log() == ["home-codex-live"]
    })
    .await;
}

#[tokio::test]
async fn a_failed_direct_dial_releases_its_selection() {
    let home = FakeHome::with(vec![live_credential()]);
    let (proxy, _, _) = start(home.clone(), None, HOUR, None).await;
    let response = wreq::Client::new()
        .websocket(format!(
            "ws{}/v1/realtime?model=fail-model",
            proxy.trim_start_matches("http")
        ))
        .send()
        .await
        .unwrap();
    assert_ne!(response.status().as_u16(), 101);
    eventually("the failed dial released its selection", || {
        home.log() == ["home-codex-live"]
    })
    .await;
}

/// The sideband relays with the call's held credential; when it ends the call ends,
/// and so does the selection.
#[tokio::test]
async fn sideband_uses_the_held_home_credential() {
    let home = FakeHome::with(vec![live_credential()]);
    let (proxy, live, seen) = start(home.clone(), Some("/v1/live/call-123"), HOUR, None).await;
    assert_eq!(post_live(&proxy, "gpt-live-1-codex").await.status().as_u16(), 201);
    let mut socket = open_socket(&proxy, "/v1/live/call-123").await;
    assert_eq!(home.requests().len(), 1, "no new Home pick for the sideband");
    let dial = seen.lock().unwrap().last().cloned().unwrap();
    assert!(dial.0.ends_with("/call-123"), "{dial:?}");
    assert_eq!(dial.1, "Bearer home-codex-live-token");
    assert!(home.log().is_empty());
    socket.send(Message::close(None)).await.unwrap();
    drop(socket);
    eventually("the finished sideband ended the call and its selection", || {
        live.calls.peek("call-123").is_none() && home.log() == ["home-codex-live"]
    })
    .await;
}

/// sideband.go: a call whose Home selection ended answers 503 and is consumed.
#[tokio::test]
async fn sideband_refuses_a_call_whose_selection_ended() {
    let home = FakeHome::with(vec![live_credential()]);
    let (proxy, live, _) = start(home.clone(), Some("/v1/live/call-123"), HOUR, None).await;
    assert_eq!(post_live(&proxy, "gpt-live-1-codex").await.status().as_u16(), 201);
    drop(live.calls.peek("call-123").unwrap().home.as_ref().unwrap().end());
    let response = wreq::Client::new()
        .get(format!("{proxy}/v1/live/call-123"))
        .header("connection", "Upgrade")
        .header("upgrade", "websocket")
        .header("sec-websocket-version", "13")
        .header("sec-websocket-key", "dGhlIHNhbXBsZSBub25jZQ==")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status().as_u16(), 503);
    assert_eq!(
        response.text().await.unwrap(),
        r#"{"error":"Codex live Home selection unavailable"}"#
    );
    assert_eq!(home.requests().len(), 1, "no new pick");
    assert!(live.calls.peek("call-123").is_none(), "the call is consumed");
}

/// A call stored without Home gets a pinned Home pick for its sideband, which ends with
/// the sideband (Go keeps it until a drain).
#[tokio::test]
async fn sideband_without_a_held_selection_picks_pinned() {
    let home = FakeHome::with(vec![credential("pinned-oauth", "codex", "oauth")]);
    let (proxy, live, seen) = start(home.clone(), None, HOUR, None).await;
    live.calls.put(
        "call-plain",
        calls::Call {
            auth_id: "pinned-oauth".into(),
            model: "gpt-live-1-codex".into(),
            ..calls::Call::default()
        },
    );
    let mut socket = open_socket(&proxy, "/v1/live/call-plain").await;
    let requests = home.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(
        (
            requests[0].kind,
            requests[0].pinned.as_str(),
            requests[0].model.as_str()
        ),
        ("websocket", "pinned-oauth", "gpt-live-1-codex")
    );
    assert_eq!(seen.lock().unwrap().last().unwrap().1, "Bearer pinned-oauth-token");
    assert!(home.log().is_empty(), "held while the sideband runs");
    socket.send(Message::close(None)).await.unwrap();
    drop(socket);
    eventually("the sideband's pick ended with it", || home.log() == ["pinned-oauth"]).await;
}

/// Go's `CloseWithReason` returns after the peers closed: the request's selection is not
/// released while unretained media is still closing.
#[tokio::test]
async fn unretained_media_close_completion_gates_the_release() {
    let home = FakeHome::with(vec![live_credential()]);
    let pending = Pending::default();
    let relay: Arc<dyn MediaRelay> = Arc::new(FakeRelay {
        fail: false,
        hang: false,
        log: home.log.clone(),
        delayed: Some(pending.clone()),
    });
    let (proxy, _, _) = start(home.clone(), None, HOUR, Some(relay)).await;
    assert_eq!(post_live(&proxy, "gpt-live-1-codex").await.status().as_u16(), 502);
    tokio::time::sleep(Duration::from_millis(20)).await;
    assert_eq!(home.log(), ["media_closed"], "released before the close finished");
    finish(&pending);
    assert_eq!(home.log(), ["media_closed", "home-codex-live"]);
}

/// capabilities.go: a hangup's temporary pick ends after the call (and its media) did.
#[tokio::test]
async fn a_hangup_pick_ends_after_the_call_media_closed() {
    let home = FakeHome::with(vec![credential("pinned-oauth", "codex", "oauth")]);
    let pending = Pending::default();
    let (proxy, live, seen) = start(home.clone(), None, HOUR, None).await;
    live.calls.put(
        "call-plain",
        calls::Call {
            auth_id: "pinned-oauth".into(),
            model: "gpt-live-1-codex".into(),
            media: Some(Arc::new(FakeSession {
                log: home.log.clone(),
                delayed: Some(pending.clone()),
            })),
            ..calls::Call::default()
        },
    );
    let hangup = wreq::Client::new()
        .post(format!("{proxy}/v1/realtime/calls/call-plain/hangup"))
        .send()
        .await
        .unwrap();
    assert_eq!(hangup.status().as_u16(), 200);
    let requests = home.requests();
    assert_eq!(
        (requests.len(), requests[0].pinned.as_str(), requests[0].kind),
        (1, "pinned-oauth", "http")
    );
    assert_eq!(seen.lock().unwrap().last().unwrap().1, "Bearer pinned-oauth-token");
    assert!(live.calls.peek("call-plain").is_none());
    assert_eq!(home.log(), ["media_closed"], "held until the media finished closing");
    finish(&pending);
    assert_eq!(home.log(), ["media_closed", "pinned-oauth"]);
}

/// sideband.go: the sideband's temporary pick ends only after the call's media finished
/// closing, as the call's own selection would.
#[tokio::test]
async fn a_sideband_pick_ends_after_the_call_media_closed() {
    let home = FakeHome::with(vec![credential("pinned-oauth", "codex", "oauth")]);
    let pending = Pending::default();
    let (proxy, live, _) = start(home.clone(), None, HOUR, None).await;
    live.calls.put(
        "call-plain",
        calls::Call {
            auth_id: "pinned-oauth".into(),
            model: "gpt-live-1-codex".into(),
            media: Some(Arc::new(FakeSession {
                log: home.log.clone(),
                delayed: Some(pending.clone()),
            })),
            ..calls::Call::default()
        },
    );
    let mut socket = open_socket(&proxy, "/v1/live/call-plain").await;
    assert_eq!(home.requests().len(), 1);
    socket.send(Message::close(None)).await.unwrap();
    drop(socket);
    eventually("the sideband ended the call", || {
        live.calls.peek("call-plain").is_none() && home.log() == ["media_closed"]
    })
    .await;
    tokio::time::sleep(Duration::from_millis(20)).await;
    assert_eq!(home.log(), ["media_closed"], "held until the media finished closing");
    finish(&pending);
    assert_eq!(home.log(), ["media_closed", "pinned-oauth"]);
}

/// A request that goes away while media setup runs leaves the lease with the setup: it
/// is released once the half-built session finished closing, not with the request.
#[tokio::test]
async fn cancelled_media_setup_releases_after_its_close() {
    for fail in [false, true] {
        let home = FakeHome::with(vec![live_credential()]);
        let pending = Pending::default();
        let relay: Arc<dyn MediaRelay> = Arc::new(FakeRelay {
            fail,
            hang: !fail,
            log: home.log.clone(),
            delayed: Some(pending.clone()),
        });
        let (proxy, _, seen) = start(home.clone(), Some("/v1/live/call-123"), HOUR, Some(relay)).await;
        let request = tokio::spawn(async move { post_live(&proxy, "gpt-live-1-codex").await });
        // Hanging: setup is running. Failing: setup closed and waits for the close.
        let in_setup = || !fail || home.log() == ["media_closed"];
        eventually("setup started", || home.requests().len() == 1 && in_setup()).await;
        tokio::time::sleep(Duration::from_millis(20)).await;
        request.abort();
        let _ = request.await;
        eventually("the cancelled setup closed its session", || {
            home.log() == ["media_closed"]
        })
        .await;
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert_eq!(
            home.log(),
            ["media_closed"],
            "fail={fail}: released before the close finished"
        );
        finish(&pending);
        assert_eq!(home.log(), ["media_closed", "home-codex-live"], "fail={fail}");
        assert!(seen.lock().unwrap().is_empty(), "nothing went upstream");
    }
}
