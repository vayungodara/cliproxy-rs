//! Response steering through the dispatch loop: the cases of Go's
//! internal/runtime/executor/codex_websockets_duplex*_test.go that need the conductor
//! (`manager.ExecuteStream`): bootstrap failover, account state after a rejection, and
//! follow-up input across attempts. The executor-only cases are cpa-exec's
//! `codex_duplex_tests.rs`. Each upstream follows the Go test's scripted conversation and
//! the assertions are Go's; fake credentials against a loopback upstream only.

use std::future::Future;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use bytes::Bytes;
use cpa_core::config::Config;
use cpa_core::credential::Credential;
use cpa_core::exec::{Caller, ExecError, ExecSession, ExecStream, FailureScope, Operation};
use cpa_core::format::Format;
use cpa_exec::Executors;
use cpa_exec::codex::{ClientFrames, SteeringInput};
use cpa_server::dispatch::{self, Call, SessionTurn};
use cpa_server::runtime::Runtime;
use futures_util::StreamExt;

/// One upstream conversation (the Go handler); failures are Go's `t.Error`.
struct Peer {
    socket: WebSocket,
    /// The dial's `Authorization` header.
    authorization: String,
    errors: Arc<Mutex<Vec<String>>>,
}

impl Peer {
    fn error(&self, message: String) {
        self.errors.lock().unwrap().push(message);
    }

    fn bad(&self) -> bool {
        self.authorization == "Bearer bad-key"
    }

    async fn read(&mut self) -> Option<String> {
        loop {
            match tokio::time::timeout(Duration::from_secs(8), self.socket.recv()).await {
                Ok(Some(Ok(Message::Text(text)))) => return Some(text.as_str().to_owned()),
                Ok(Some(Ok(Message::Ping(_) | Message::Pong(_)))) => continue,
                _ => return None,
            }
        }
    }

    async fn write(&mut self, payload: &str) {
        if self.socket.send(Message::Text(payload.into())).await.is_err() {
            self.error(format!("upstream write: {payload}"));
        }
    }

    async fn idle(&mut self) {
        let _ = tokio::time::timeout(Duration::from_secs(8), self.socket.recv()).await;
    }
}

struct Upstream {
    url: String,
    errors: Arc<Mutex<Vec<String>>>,
    dials: Arc<AtomicUsize>,
    done: Arc<AtomicUsize>,
}

impl Upstream {
    async fn start<F, Fut>(script: F) -> Self
    where
        F: Fn(Peer) -> Fut + Clone + Send + Sync + 'static,
        Fut: Future<Output = ()> + Send + 'static,
    {
        let errors = Arc::new(Mutex::new(Vec::new()));
        let (dials, done) = (Arc::new(AtomicUsize::new(0)), Arc::new(AtomicUsize::new(0)));
        let state = (script, errors.clone(), dials.clone(), done.clone());
        let app = axum::Router::new().fallback(move |ws: WebSocketUpgrade, headers: axum::http::HeaderMap| {
            let (script, errors, dials, done) = state.clone();
            async move {
                dials.fetch_add(1, Ordering::SeqCst);
                let authorization = headers
                    .get("authorization")
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or_default()
                    .to_owned();
                ws.on_upgrade(move |socket| async move {
                    script(Peer {
                        socket,
                        authorization,
                        errors,
                    })
                    .await;
                    done.fetch_add(1, Ordering::SeqCst);
                })
            }
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await });
        Self {
            url,
            errors,
            dials,
            done,
        }
    }

    /// Every socket was closed by the proxy and no conversation failed.
    async fn finish(&self) {
        let started = Instant::now();
        while self.done.load(Ordering::SeqCst) < self.dials.load(Ordering::SeqCst) {
            assert!(started.elapsed() < Duration::from_secs(3), "socket cleanup stalled");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let errors = self.errors.lock().unwrap().clone();
        assert!(errors.is_empty(), "upstream: {errors:?}");
    }
}

/// Go's manager with `codex.response-steering`, the given API keys (priority descending
/// as listed) and the session's client queue.
struct Conductor {
    rt: Arc<Runtime>,
    input: tokio::sync::mpsc::Sender<Bytes>,
    session: String,
    model: String,
    _auth_dir: AuthDir,
}

/// A private, empty `auth-dir`, removed when dropped: credential loading never falls back
/// to `~/.cli-proxy-api`.
struct AuthDir(std::path::PathBuf);

impl AuthDir {
    fn new() -> Self {
        let dir = std::env::temp_dir().join(format!("cpa-it-auth-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }
}

impl Drop for AuthDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

impl Conductor {
    fn new(upstream: &str, keys: &[&str], model: &str, session: &str) -> Self {
        let auth_dir = AuthDir::new();
        let mut yaml = format!(
            "auth-dir: {}\ncodex:\n  response-steering: true\ncodex-api-key:\n",
            auth_dir.0.display()
        );
        for (i, key) in keys.iter().enumerate() {
            yaml.push_str(&format!(
                "  - api-key: {key}\n    base-url: {upstream}\n    websockets: true\n    priority: {}\n    models:\n      - name: {model}\n",
                4 - i
            ));
        }
        let cfg = Config::parse(&yaml).unwrap();
        let credentials = cpa_core::config::credentials::from_config(&cfg);
        let executors = Executors {
            claude: cpa_exec::claude::ClaudeExecutor::new("http://127.0.0.1:1").unwrap(),
            codex: cpa_exec::codex::CodexExecutor::new().unwrap(),
            devices: Default::default(),
            openai: Default::default(),
            google: Default::default(),
        };
        let rt = Arc::new(cpa_server::testing::runtime(cfg, credentials, executors));
        let (input, rx) = tokio::sync::mpsc::channel(1);
        let frames: ClientFrames = Arc::new(tokio::sync::Mutex::new(rx));
        rt.executors
            .codex
            .attach_steering(session, SteeringInput::new(frames, |_| true));
        Self {
            rt,
            input,
            session: session.to_owned(),
            model: model.to_owned(),
            _auth_dir: auth_dir,
        }
    }

    fn credential(&self, key: &str) -> Arc<Credential> {
        self.rt
            .store()
            .snapshot()
            .into_iter()
            .find(|c| c.attributes.get("api_key").map(String::as_str) == Some(key))
            .unwrap()
    }

    /// `manager.ExecuteStream` for a downstream WebSocket turn: the first item, then the
    /// rest, as one stream.
    async fn stream(&self) -> ExecStream {
        let call = Call {
            entry: Format::OpenAIResponse,
            response: Format::OpenAIResponse,
            operation: Operation::Generate,
            model: self.model.clone(),
            body: Bytes::from(format!(r#"{{"model":"{}","input":[]}}"#, self.model)),
            stream: true,
            alt: None,
            headers: Default::default(),
            caller: Caller {
                principal: "client-key".into(),
                source: "authorization",
            },
            forced_provider: None,
            selection_model: None,
            execution_session: Some(self.session.clone()),
            request_path: "/v1/responses".into(),
            peer: None,
            turn: Some(Arc::new(SessionTurn {
                session: ExecSession {
                    id: self.session.clone(),
                    continuation: false,
                    lease: None,
                },
                pinned: None,
                on_selected: None,
                home: None,
            })),
            media: None,
        };
        let done = dispatch::run_with_bootstrap_retries(&self.rt, call, &dispatch::Trace::default())
            .await
            .unwrap_or_else(|f| panic!("dispatch failed: {}", f.text()));
        let dispatch::Done::Stream { first, rest, .. } = done else {
            panic!("a stream");
        };
        futures_util::stream::iter(first.map(Ok)).chain(rest).boxed()
    }

    /// Go's `input <- payload`; returns once the duplex took it.
    async fn send(&self, payload: &str) {
        self.input.send(Bytes::from(payload.to_owned())).await.unwrap();
        let started = Instant::now();
        while self.input.capacity() < self.input.max_capacity() {
            assert!(
                started.elapsed() < Duration::from_secs(5),
                "the duplex never read the frame"
            );
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
    }

    /// Go `ModelStates[model]`: unavailable with this status (and, for a quota, the
    /// credential-wide `credential_quota` cooldown of at least the upstream's hour).
    fn assert_cooled(&self, key: &str, status: u16, quota: bool, name: &str) {
        let credential = self.credential(key);
        assert!(
            self.rt.store().blocked(&credential, &self.model),
            "{name}: failure not recorded"
        );
        let cooldowns = self.rt.store().cooldowns(&credential.id);
        assert!(
            cooldowns.iter().any(|c| c.status == status),
            "{name}: last error status lost: {cooldowns:?}"
        );
        if quota {
            assert!(
                cooldowns
                    .iter()
                    .any(|c| c.model.is_empty() && c.quota && c.remaining > Duration::from_secs(3590)),
                "{name}: credential quota cooldown lost: {cooldowns:?}"
            );
        }
    }
}

async fn next(stream: &mut ExecStream) -> Option<Result<String, ExecError>> {
    tokio::time::timeout(Duration::from_secs(10), stream.next())
        .await
        .expect("stream stalled")
        .map(|item| item.map(|b| String::from_utf8_lossy(&b).into_owned()))
}

fn get(payload: &str, path: &str) -> String {
    gjson::get(payload, path).str().to_owned()
}

/// `TestCodexDuplexInitialFailure` with failover: the rejected account is cooled with
/// the upstream's status and quota delay, and only the healthy account's events escape.
#[tokio::test]
async fn initial_failure_fails_over() {
    let cases: [(&str, &str, u16, bool); 4] = [
        (
            "response_failed_auth",
            r#"{"type":"response.failed","response":{"error":{"type":"authentication_error","message":"expired credential"}}}"#,
            401,
            false,
        ),
        (
            "response_failed_quota",
            r#"{"type":"response.failed","response":{"error":{"type":"usage_limit_reached","message":"quota exhausted","resets_in_seconds":3600}}}"#,
            429,
            true,
        ),
        (
            "error_auth",
            r#"{"type":"error","status":401,"headers":{"X-Request-Id":"initial-rejection"},"error":{"type":"server_error","message":"expired credential"}}"#,
            401,
            false,
        ),
        (
            "error_quota",
            r#"{"type":"error","status_code":429,"headers":{"X-Request-Id":"initial-rejection"},"error":{"type":"usage_limit_reached","message":"quota exhausted","resets_in_seconds":3600}}"#,
            429,
            true,
        ),
    ];
    for (name, payload, status, quota) in cases {
        let (rejected, succeeded) = (Arc::new(AtomicUsize::new(0)), Arc::new(AtomicUsize::new(0)));
        let (r, s) = (rejected.clone(), succeeded.clone());
        let up = Upstream::start(move |mut c: Peer| {
            let (rejected, succeeded) = (r.clone(), s.clone());
            async move {
                c.read().await;
                if c.bad() {
                    rejected.fetch_add(1, Ordering::SeqCst);
                    c.write(payload).await;
                } else {
                    succeeded.fetch_add(1, Ordering::SeqCst);
                    c.write(r#"{"type":"response.created","response":{"id":"healthy-response","output":[]}}"#)
                        .await;
                    c.write(r#"{"type":"response.completed","response":{"id":"healthy-response","output":[]}}"#)
                        .await;
                }
                // Keep the socket open: the executor must terminate it itself.
                c.idle().await;
            }
        })
        .await;
        let conductor = Conductor::new(
            &up.url,
            &["bad-key", "good-key"],
            "duplex-initial-failure-model",
            &format!("initial-{name}"),
        );
        let mut stream = conductor.stream().await;
        let mut completed = false;
        while let Some(chunk) = next(&mut stream).await {
            let chunk = chunk.unwrap_or_else(|e| panic!("{name}: {e}"));
            match get(&chunk, "type").as_str() {
                "response.completed" => {
                    completed = get(&chunk, "response.id") == "healthy-response";
                    break;
                }
                "response.created" => {}
                _ => panic!("{name}: rejected account payload escaped bootstrap: {chunk}"),
            }
        }
        drop(stream);
        assert!(completed, "{name}: healthy account never completed");
        conductor.assert_cooled("bad-key", status, quota, name);
        up.finish().await;
        assert_eq!(
            (rejected.load(Ordering::SeqCst), succeeded.load(Ordering::SeqCst)),
            (1, 1),
            "{name}: attempts (rejected, healthy)"
        );
    }
}

/// `TestCodexDuplexBootstrapPreservesFollowup`: a frame queued before bootstrap is never
/// sent on the rejected account and reaches the healthy one unchanged.
#[tokio::test]
async fn bootstrap_preserves_followup() {
    for kind in ["response.steer", "response.create"] {
        let counts: Arc<[AtomicUsize; 4]> = Arc::new(Default::default());
        let seen = counts.clone();
        let up = Upstream::start(move |mut c: Peer| {
            let counts = seen.clone();
            async move {
                // [rejected, succeeded, rejected follow-ups, healthy follow-ups]
                if c.read().await.is_none() {
                    c.error("initial read".into());
                    return;
                }
                if c.bad() {
                    counts[0].fetch_add(1, Ordering::SeqCst);
                    c.write(r#"{"type":"error","status":401,"error":{"type":"authentication_error","message":"expired credential"}}"#).await;
                    // Bootstrap cleanup must close this socket without forwarding input.
                    while c.read().await.is_some() {
                        counts[2].fetch_add(1, Ordering::SeqCst);
                    }
                    return;
                }
                counts[1].fetch_add(1, Ordering::SeqCst);
                c.write(r#"{"type":"response.created","response":{"id":"healthy","output":[]}}"#)
                    .await;
                let Some(payload) = c.read().await else {
                    c.error("healthy account did not receive queued follow-up".into());
                    return;
                };
                counts[3].fetch_add(1, Ordering::SeqCst);
                if get(&payload, "type") != kind || get(&payload, "input.0.content.0.text") != "PRESERVE_ME" {
                    c.error(format!("follow-up changed: {payload}"));
                }
                c.write(r#"{"type":"response.completed","response":{"id":"healthy","output":[]}}"#)
                    .await;
                c.idle().await;
            }
        })
        .await;
        let conductor = Conductor::new(
            &up.url,
            &["bad-key", "good-key"],
            "duplex-bootstrap-followup-model",
            &format!("followup-{kind}"),
        );
        conductor
            .input
            .send(Bytes::from(format!(
                r#"{{"type":"{kind}","input":[{{"role":"user","content":[{{"type":"input_text","text":"PRESERVE_ME"}}]}}]}}"#
            )))
            .await
            .unwrap();
        let mut stream = conductor.stream().await;
        let mut completed = false;
        while let Some(chunk) = next(&mut stream).await {
            let chunk = chunk.unwrap_or_else(|e| panic!("{kind}: {e}"));
            if get(&chunk, "type") == "response.completed" {
                completed = true;
                break;
            }
        }
        drop(stream);
        assert!(completed, "{kind}: healthy account never completed the follow-up");
        up.finish().await;
        let counts: Vec<usize> = counts.iter().map(|c| c.load(Ordering::SeqCst)).collect();
        assert_eq!(
            counts,
            [1, 1, 0, 1],
            "{kind}: attempts rejected/healthy, follow-ups rejected/healthy"
        );
    }
}

/// `TestCodexDuplexLaterCredentialFailure`: a 401/403/429 after the first response
/// forwards the event, ends the stream with the original classification, cools that
/// account only, and is never replayed on another one.
#[tokio::test]
async fn later_credential_failure() {
    for kind in ["error", "response.failed"] {
        for status in [401u16, 403, 429] {
            for queued in [false, true] {
                let name = format!("{kind}/{status}/queued={queued}");
                let attempts = Arc::new(AtomicUsize::new(0));
                let seen = attempts.clone();
                let up = Upstream::start(move |mut c: Peer| {
                    let attempts = seen.clone();
                    async move {
                        attempts.fetch_add(1, Ordering::SeqCst);
                        c.read().await;
                        c.write(r#"{"type":"response.created","response":{"id":"started","output":[]}}"#)
                            .await;
                        if queued {
                            if c.read().await.is_none() {
                                c.error("queued create".into());
                                return;
                            }
                        } else {
                            c.write(r#"{"type":"response.completed","response":{"id":"started","output":[]}}"#)
                                .await;
                        }
                        let error_type = match status {
                            403 => "permission_error",
                            429 => "usage_limit_reached",
                            _ => "authentication_error",
                        };
                        let body = format!(
                            r#"{{"type":"{error_type}","status":{status},"message":"credential rejected","resets_in_seconds":3600}}"#
                        );
                        if kind == "error" {
                            c.write(&format!(
                                r#"{{"type":"error","status":{status},"headers":{{"X-Request-Id":"later-rejection"}},"error":{body}}}"#
                            ))
                            .await;
                        } else {
                            c.write(&format!(
                                r#"{{"type":"response.failed","response":{{"id":"started","error":{body}}}}}"#
                            ))
                            .await;
                        }
                        // The proxy must terminate this still-open socket on its own.
                        c.idle().await;
                    }
                })
                .await;
                let conductor = Conductor::new(
                    &up.url,
                    &["bad-key", "good-key"],
                    "later-credential-model",
                    &format!("later-{kind}-{status}-{queued}"),
                );
                let mut stream = conductor.stream().await;
                let (mut payload_seen, mut terminal_seen) = (false, false);
                while let Some(chunk) = next(&mut stream).await {
                    let chunk = match chunk {
                        Ok(chunk) => chunk,
                        Err(error) => {
                            terminal_seen = true;
                            assert_eq!(error.status, status, "{name}: terminal classification lost: {error}");
                            assert_ne!(
                                error.scope,
                                FailureScope::Request,
                                "{name}: credential failure became request-scoped"
                            );
                            assert!(
                                payload_seen,
                                "{name}: original failure not forwarded before terminal error"
                            );
                            continue;
                        }
                    };
                    let event = get(&chunk, "type");
                    if event == "response.created" && queued {
                        conductor.send(r#"{"type":"response.create","input":[]}"#).await;
                    }
                    payload_seen |= event == kind;
                }
                assert!(
                    payload_seen && terminal_seen,
                    "{name}: failure payload={payload_seen} terminal={terminal_seen}"
                );
                conductor.assert_cooled("bad-key", status, status == 429, &name);
                let healthy = conductor.credential("good-key");
                assert!(
                    conductor.rt.store().cooldowns(&healthy.id).is_empty(),
                    "{name}: unrelated healthy account changed"
                );
                up.finish().await;
                assert_eq!(
                    attempts.load(Ordering::SeqCst),
                    1,
                    "{name}: started response replayed across attempts"
                );
            }
        }
    }
}

/// `TestCodexDuplexConnectionTimeoutDoesNotCoolHealthyAccount`: the downstream input
/// failing after a successful response ends the stream with a connection error and
/// leaves the account and model available. (Go injects a read timeout through the
/// input; the queue here only closes, as Go's own downstream reader does on any error.)
#[tokio::test]
async fn connection_failure_does_not_cool_the_account() {
    let up = Upstream::start(|mut c: Peer| async move {
        c.read().await;
        c.write(r#"{"type":"response.created","response":{"id":"r1"}}"#).await;
        c.write(r#"{"type":"response.completed","response":{"id":"r1","output":[]}}"#)
            .await;
        c.idle().await;
    })
    .await;
    let mut conductor = Conductor::new(&up.url, &["test-key"], "duplex-health-model", "duplex-health-session");
    let mut stream = conductor.stream().await;
    let (mut completed, mut failure) = (false, None);
    while let Some(chunk) = next(&mut stream).await {
        match chunk {
            Ok(chunk) if get(&chunk, "type") == "response.completed" => {
                completed = true;
                let (closed, _) = tokio::sync::mpsc::channel(1);
                drop(std::mem::replace(&mut conductor.input, closed));
            }
            Ok(_) => {}
            Err(error) => failure = Some(error),
        }
    }
    let failure = failure.expect("the stream ends with the connection error");
    assert!(completed);
    assert_eq!(failure.scope, FailureScope::Request, "{failure}");
    let credential = conductor.credential("test-key");
    assert!(
        !conductor.rt.store().blocked(&credential, "duplex-health-model"),
        "connection failure cooled the account"
    );
    assert!(conductor.rt.store().cooldowns(&credential.id).is_empty());
    up.finish().await;
}
