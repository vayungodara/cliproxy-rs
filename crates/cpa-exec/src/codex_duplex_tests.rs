//! Response steering on the executor, ported case for case from Go's
//! internal/runtime/executor/codex_websockets_duplex*_test.go. Each upstream follows the Go
//! test's scripted conversation and the assertions are Go's. Cases that need the
//! conductor (failover, account state) live in cpa-server's `tests/duplex_dispatch.rs`.
//!
//! Go hands the executor an input channel; here the client's frames go through the
//! session's shared queue. Go's unbuffered sends become a send plus [`delivered`], which
//! waits until the duplex took the frame.

use super::*;
use std::future::Future;
use std::sync::atomic::{AtomicUsize, Ordering};

use futures_util::stream::BoxStream;

const STEERING: &str = "codex:\n  response-steering: true\n";
const MODEL: &str = "gpt-6-astra";

/// One upstream conversation, as the Go test's handler runs it. Failures are collected
/// (Go's `t.Error` in the handler) and checked by [`Scripted::finish`].
struct Peer {
    socket: AxSocket,
    errors: Arc<Mutex<Vec<String>>>,
}

impl Peer {
    fn error(&self, message: String) {
        self.errors.lock().unwrap().push(message);
    }

    /// `c.ReadMessage()` with the Go test's read deadline.
    async fn read(&mut self) -> String {
        loop {
            match tokio::time::timeout(Duration::from_secs(10), self.socket.recv()).await {
                Ok(Some(Ok(AxMessage::Text(text)))) => return text.as_str().to_owned(),
                Ok(Some(Ok(AxMessage::Ping(_) | AxMessage::Pong(_)))) => continue,
                other => {
                    self.error(format!("upstream read: {other:?}"));
                    return String::new();
                }
            }
        }
    }

    async fn write(&mut self, payload: &str) {
        if self.socket.send(AxMessage::Text(payload.into())).await.is_err() {
            self.error(format!("upstream write: {payload}"));
        }
    }

    /// `_, _, _ = c.ReadMessage()`: hold the socket until the proxy sends or closes.
    async fn idle(&mut self) {
        let _ = tokio::time::timeout(Duration::from_secs(10), self.socket.recv()).await;
    }
}

/// A loopback upstream running `script` for every dial.
struct Scripted {
    url: String,
    errors: Arc<Mutex<Vec<String>>>,
    dials: Arc<AtomicUsize>,
    done: Arc<AtomicUsize>,
}

impl Scripted {
    async fn start<F, Fut>(script: F) -> Self
    where
        F: Fn(Peer) -> Fut + Clone + Send + Sync + 'static,
        Fut: Future<Output = ()> + Send + 'static,
    {
        let errors = Arc::new(Mutex::new(Vec::new()));
        let (dials, done) = (Arc::new(AtomicUsize::new(0)), Arc::new(AtomicUsize::new(0)));
        let state = (script, errors.clone(), dials.clone(), done.clone());
        let app = axum::Router::new().fallback(move |ws: WebSocketUpgrade| {
            let (script, errors, dials, done) = state.clone();
            async move {
                dials.fetch_add(1, Ordering::SeqCst);
                ws.on_upgrade(move |socket| async move {
                    script(Peer { socket, errors }).await;
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

    /// Every conversation ended (the proxy closed its socket) and none failed.
    async fn finish(&self) {
        let started = Instant::now();
        while self.done.load(Ordering::SeqCst) < self.dials.load(Ordering::SeqCst) {
            assert!(started.elapsed() < Duration::from_secs(3), "upstream cleanup stalled");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let errors = self.errors.lock().unwrap().clone();
        assert!(errors.is_empty(), "upstream: {errors:?}");
    }
}

/// A steering executor with the session's client queue (`WithWebsocketInput`).
struct Client {
    executor: CodexExecutor,
    input: tokio::sync::mpsc::Sender<Bytes>,
    frames: ClientFrames,
    session: String,
}

impl Client {
    fn new(session: &str) -> Self {
        let executor = CodexExecutor::new().unwrap();
        let (input, rx) = tokio::sync::mpsc::channel(1);
        let frames: ClientFrames = Arc::new(tokio::sync::Mutex::new(rx));
        executor.attach_steering(session, SteeringInput::new(frames.clone(), |_| true));
        Self {
            executor,
            input,
            frames,
            session: session.to_owned(),
        }
    }

    /// `exec.ExecuteStream` with Go's `opts.SourceFormat` (the response format follows it).
    async fn start(&self, url: &str, body: &str, source: Format) -> BoxStream<'static, Result<Bytes, ExecError>> {
        self.start_with(url, body, source, Default::default()).await
    }

    async fn start_with(
        &self,
        url: &str,
        body: &str,
        source: Format,
        usage: cpa_core::exec::UsageSink,
    ) -> BoxStream<'static, Result<Bytes, ExecError>> {
        let mut req = request(body);
        req.usage = usage;
        req.model = MODEL.into();
        req.requested_model = MODEL.into();
        req.source_format = source;
        req.response_format = source;
        req.execution_session = Some(self.session.clone());
        let session = ExecSession {
            id: self.session.clone(),
            continuation: false,
            lease: None,
        };
        let cfg = Config::parse(STEERING).unwrap();
        let response = self
            .executor
            .execute_in_session(&credential(url), req, &cfg, &session)
            .await
            .unwrap();
        let ResponseBody::Stream(stream) = response.body else {
            panic!("websocket turns stream");
        };
        stream
    }

    /// `input <- payload` on Go's unbuffered channel: returns once the duplex took it.
    async fn send(&self, payload: &str) {
        self.input.send(Bytes::from(payload.to_owned())).await.unwrap();
        delivered(&self.input).await;
    }
}

async fn delivered(input: &tokio::sync::mpsc::Sender<Bytes>) {
    let started = Instant::now();
    while input.capacity() < input.max_capacity() {
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "the duplex never read the frame"
        );
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
}

async fn next(stream: &mut BoxStream<'static, Result<Bytes, ExecError>>) -> Option<Result<String, ExecError>> {
    tokio::time::timeout(Duration::from_secs(10), stream.next())
        .await
        .expect("stream stalled")
        .map(|item| item.map(|b| String::from_utf8_lossy(&b).into_owned()))
}

fn get(payload: &str, path: &str) -> String {
    gjson::get(payload, path).str().to_owned()
}

// ------------------------------------------------------------------ codex_websockets_duplex_test.go

/// `TestCodexDuplexSteeringLifecycle`: the upstream cannot finish the first response
/// until it reads the steer, so a serial request/response proxy deadlocks.
#[tokio::test]
async fn steering_lifecycle() {
    for boundary in ["response.incomplete", "response.completed"] {
        const STEER: &str = r#"{"type":"response.steer","previous_response_id":"r1","input":[{"role":"user","content":[{"type":"input_text","text":"STEER_OK"}]}]}"#;
        const ACCEPTED: &str =
            r#"{"type":"response.steer.accepted","sequence_number":2,"steer":{"id":"s1","previous_response_id":"r1"}}"#;
        const PENDING: &str = r#"{"type":"response.steer.pending","sequence_number":8,"steer":{"id":"s2","previous_response_id":"r2"},"reason":"waiting_for_required_input","required_input":[{"type":"function_call_output","call_id":"c1","name":"lookup"}]}"#;
        const FAILED: &str = r#"{"type":"response.steer.failed","sequence_number":14,"steer":{"id":"s3","previous_response_id":"missing","input":"recover me"},"error":{"code":"response_not_found","message":"missing response"}}"#;
        let up = Scripted::start(move |mut c: Peer| async move {
            let first = c.read().await;
            if get(&first, "type") != "response.create" {
                c.error(format!("initial create: {first}"));
            }
            c.write(r#"{"type":"response.created","response":{"id":"r1","output":[]}}"#).await;
            let got = c.read().await;
            if got != STEER {
                c.error(format!("steer transformed: {got}"));
            }
            c.write(ACCEPTED).await;
            c.write(&format!(
                r#"{{"type":"{boundary}","response":{{"id":"r1","output":[],"incomplete_details":{{"reason":"steered"}}}}}}"#
            ))
            .await;
            c.write(r#"{"type":"response.created","response":{"id":"r2","output":[]}}"#).await;
            let got = c.read().await;
            if get(&got, "type") != "response.steer" {
                c.error(format!("second control: {got}"));
            }
            c.write(r#"{"type":"response.steer.accepted","steer":{"id":"s2","previous_response_id":"r2"}}"#)
                .await;
            c.write(r#"{"type":"response.completed","response":{"id":"r2","output":[{"type":"function_call","call_id":"c1","name":"lookup","arguments":"{}"}]}}"#).await;
            c.write(PENDING).await;
            let got = c.read().await;
            if get(&got, "type") != "response.create"
                || get(&got, "previous_response_id") != "r2"
                || get(&got, "input.0.call_id") != "c1"
            {
                c.error(format!("tool continuation: {got}"));
            }
            if get(&got, "instructions") != "New settings" {
                c.error(format!("explicit settings not applied: {got}"));
            }
            c.write(r#"{"type":"response.created","response":{"id":"r3","output":[]}}"#).await;
            c.write(r#"{"type":"response.output_item.done","output_index":0,"item":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"STEER_OK"}]}}"#).await;
            c.write(r#"{"type":"response.completed","response":{"id":"r3","output":[]}}"#).await;
            // Idle steering must also reach the same live upstream connection.
            let got = c.read().await;
            if get(&got, "previous_response_id") != "missing" {
                c.error(format!("idle control missing: {got}"));
            }
            c.write(FAILED).await;
            c.idle().await;
        })
        .await;
        let client = Client::new(&format!("duplex-test-{boundary}"));
        let mut stream = client
            .start(
                &up.url,
                r#"{"model":"gpt-6-astra","input":[],"instructions":"Initial settings"}"#,
                Format::Codex,
            )
            .await;
        let mut seen = std::collections::HashSet::new();
        while !seen.contains("failed") {
            let chunk = next(&mut stream)
                .await
                .expect("stream ended before all responses")
                .unwrap();
            let (event, id) = (get(&chunk, "type"), get(&chunk, "response.id"));
            match (event.as_str(), id.as_str()) {
                ("response.created", "r1") => client.send(STEER).await,
                ("response.steer.accepted", _) if get(&chunk, "steer.id") == "s1" => {
                    assert_eq!(chunk, ACCEPTED, "accepted event changed");
                    seen.insert("accepted");
                }
                (e, "r1") if e == boundary => {
                    seen.insert("boundary");
                }
                ("response.created", "r2") => {
                    client
                        .send(r#"{"type":"response.steer","previous_response_id":"r2","input":"Use tool result"}"#)
                        .await;
                }
                ("response.steer.pending", _) => {
                    assert_eq!(chunk, PENDING, "pending event changed");
                    seen.insert("pending");
                    client.send(r#"{"type":"response.create","previous_response_id":"r2","instructions":"New settings","input":[{"type":"function_call_output","call_id":"c1","output":"ok"}]}"#).await;
                }
                ("response.completed", "r3") => {
                    assert_eq!(
                        get(&chunk, "response.output.0.content.0.text"),
                        "STEER_OK",
                        "successor output missing or contaminated by preceding response"
                    );
                    seen.insert("successor");
                    client
                        .send(r#"{"type":"response.steer","previous_response_id":"missing","input":"recover me"}"#)
                        .await;
                }
                ("response.steer.failed", _) => {
                    assert_eq!(chunk, FAILED, "failed input changed");
                    seen.insert("failed");
                }
                _ => {}
            }
        }
        drop(stream);
        up.finish().await;
        assert_eq!(up.dials.load(Ordering::SeqCst), 1, "connections");
        for key in ["accepted", "boundary", "pending", "successor", "failed"] {
            assert!(seen.contains(key), "{boundary}: missing {key}");
        }
    }
}

/// `TestCodexDuplexAppendInheritsContextAndInstructions`.
#[tokio::test]
async fn append_inherits_context_and_instructions() {
    let up = Scripted::start(|mut c: Peer| async move {
        let first = c.read().await;
        if get(&first, "instructions") != "Initial system instructions" {
            c.error(format!("first instructions: {first}"));
        }
        c.write(r#"{"type":"response.created","response":{"id":"resp-1","output":[]}}"#)
            .await;
        c.write(r#"{"type":"response.completed","response":{"id":"resp-1","output":[]}}"#)
            .await;
        let second = c.read().await;
        for (path, want) in [
            ("type", "response.create"),
            ("previous_response_id", "resp-1"),
            ("model", "gpt-6-astra"),
            ("instructions", "Initial system instructions"),
        ] {
            if get(&second, path) != want {
                c.error(format!("second {path} = {:?}, want {want}", get(&second, path)));
            }
        }
        c.write(r#"{"type":"response.created","response":{"id":"resp-2","output":[]}}"#)
            .await;
        c.write(r#"{"type":"response.completed","response":{"id":"resp-2","output":[]}}"#)
            .await;
        c.idle().await;
    })
    .await;
    let client = Client::new("append-test");
    let mut stream = client
        .start(
            &up.url,
            r#"{"model":"gpt-6-astra","input":[{"role":"user","content":"turn 1"}],"instructions":"Initial system instructions"}"#,
            Format::Codex,
        )
        .await;
    while let Some(chunk) = next(&mut stream).await {
        let chunk = chunk.unwrap();
        let (event, id) = (get(&chunk, "type"), get(&chunk, "response.id"));
        if event == "response.completed" && id == "resp-1" {
            // previous_response_id, model and instructions all omitted.
            client
                .send(r#"{"type":"response.append","input":[{"role":"user","content":"turn 2"}]}"#)
                .await;
        }
        if event == "response.completed" && id == "resp-2" {
            break;
        }
    }
    drop(stream);
    up.finish().await;
}

/// `TestCodexDuplexStandaloneCreateDoesNotInheritParentID`.
#[tokio::test]
async fn standalone_create_does_not_inherit_parent_id() {
    let up = Scripted::start(|mut c: Peer| async move {
        c.read().await;
        c.write(r#"{"type":"response.created","response":{"id":"resp-1","output":[]}}"#)
            .await;
        c.write(r#"{"type":"response.completed","response":{"id":"resp-1","output":[]}}"#)
            .await;
        let second = c.read().await;
        if get(&second, "type") != "response.create" {
            c.error(format!("second type: {second}"));
        }
        if gjson::get(&second, "previous_response_id").exists() {
            c.error(format!(
                "standalone create must not gain previous_response_id: {second}"
            ));
        }
        c.write(r#"{"type":"response.created","response":{"id":"resp-2","output":[]}}"#)
            .await;
        c.write(r#"{"type":"response.completed","response":{"id":"resp-2","output":[]}}"#)
            .await;
        c.idle().await;
    })
    .await;
    let client = Client::new("standalone-test");
    let mut stream = client
        .start(
            &up.url,
            r#"{"model":"gpt-6-astra","input":[{"role":"user","content":"turn 1"}]}"#,
            Format::Codex,
        )
        .await;
    while let Some(chunk) = next(&mut stream).await {
        let chunk = chunk.unwrap();
        let (event, id) = (get(&chunk, "type"), get(&chunk, "response.id"));
        if event == "response.completed" && id == "resp-1" {
            client
                .send(r#"{"type":"response.create","model":"gpt-6-astra","input":[{"role":"user","content":"turn 2 standalone"}]}"#)
                .await;
        }
        if event == "response.completed" && id == "resp-2" {
            break;
        }
    }
    drop(stream);
    up.finish().await;
}

/// `TestCodexDuplexQueuedCreateDoesNotBlockSubsequentSteer`.
#[tokio::test]
async fn queued_create_does_not_block_subsequent_steer() {
    let up = Scripted::start(|mut c: Peer| async move {
        let first = c.read().await;
        if get(&first, "type") != "response.create" {
            c.error(format!("expected response.create: {first}"));
        }
        c.write(r#"{"type":"response.created","response":{"id":"r1","output":[]}}"#).await;
        let steer1 = c.read().await;
        if get(&steer1, "type") != "response.steer" {
            c.error(format!("expected response.steer: {steer1}"));
        }
        c.write(r#"{"type":"response.steer.accepted","steer":{"id":"s1","previous_response_id":"r1"}}"#)
            .await;
        c.write(r#"{"type":"response.incomplete","response":{"id":"r1","output":[],"incomplete_details":{"reason":"steered"}}}"#).await;
        // The automatic successor holds back the next create.
        c.write(r#"{"type":"response.created","response":{"id":"auto-1","previous_response_id":"r1","output":[]}}"#)
            .await;
        // The steer the client sent after that create must arrive first.
        let steer = c.read().await;
        if get(&steer, "type") != "response.steer" {
            c.error(format!("expected response.steer to arrive before blocked create, got: {steer}"));
            return;
        }
        c.write(r#"{"type":"response.steer.accepted","steer":{"id":"s2","previous_response_id":"auto-1"}}"#)
            .await;
        c.write(r#"{"type":"response.completed","response":{"id":"auto-1","output":[]}}"#).await;
        c.write(r#"{"type":"response.created","response":{"id":"auto-2","previous_response_id":"auto-1","output":[]}}"#)
            .await;
        c.write(r#"{"type":"response.completed","response":{"id":"auto-2","output":[]}}"#).await;
        let queued = c.read().await;
        if get(&queued, "type") != "response.create" {
            c.error(format!("queued create: {queued}"));
        }
        c.write(r#"{"type":"response.created","response":{"id":"r2","output":[]}}"#).await;
        c.write(r#"{"type":"response.completed","response":{"id":"r2","output":[]}}"#).await;
        c.idle().await;
    })
    .await;
    let client = Client::new("steer-block-test");
    let mut stream = client
        .start(
            &up.url,
            r#"{"model":"gpt-6-astra","input":[{"role":"user","content":"start"}]}"#,
            Format::Codex,
        )
        .await;
    while let Some(chunk) = next(&mut stream).await {
        let chunk = chunk.unwrap();
        let (event, id) = (get(&chunk, "type"), get(&chunk, "response.id"));
        if event == "response.created" && id == "r1" {
            client
                .send(r#"{"type":"response.steer","previous_response_id":"r1","input":"steer 1"}"#)
                .await;
        }
        if event == "response.created" && id == "auto-1" {
            client.send(r#"{"type":"response.create","model":"gpt-6-astra","previous_response_id":"auto-1","input":[{"role":"user","content":"queued next"}]}"#).await;
            client
                .send(r#"{"type":"response.steer","previous_response_id":"auto-1","input":"steer 2 in flight"}"#)
                .await;
        }
        if event == "response.completed" && id == "r2" {
            break;
        }
    }
    drop(stream);
    up.finish().await;
}

// ------------------------------------------------- codex_websockets_duplex_successor_metadata_test.go

/// `TestCodexDuplexAutomaticSuccessorMetadata`: a create queued behind steering must not
/// supply an automatic successor's settings while it waits to be sent.
#[tokio::test]
async fn automatic_successor_metadata() {
    for scenario in [
        "completed",
        "incomplete",
        "early_tool_result",
        "failed_steering",
        "multiple_steers",
        "create_before_steer",
    ] {
        let queued = Arc::new(tokio::sync::Notify::new());
        let before_steer = Arc::new(tokio::sync::Notify::new());
        let (q, b) = (queued.clone(), before_steer.clone());
        let up = Scripted::start(move |mut c: Peer| {
            let (queued, before_steer) = (q.clone(), b.clone());
            async move {
                let response = |kind: &str, id: &str, key: &str| {
                    let parent = if id == "automatic" { "first" } else { "" };
                    format!(
                        r#"{{"type":"{kind}","response":{{"id":"{id}","previous_response_id":"{parent}","prompt_cache_key":"{key}","output":[],"incomplete_details":{{"reason":"steered"}}}}}}"#
                    )
                };
                let first = c.read().await;
                let first_key = get(&first, "prompt_cache_key");
                c.write(&response("response.created", "first", &first_key)).await;
                if scenario == "create_before_steer" {
                    let middle = c.read().await;
                    if get(&middle, "type") != "response.create" {
                        c.error(format!("expected middle create: {middle}"));
                        return;
                    }
                    if tokio::time::timeout(Duration::from_secs(3), before_steer.notified())
                        .await
                        .is_err()
                    {
                        c.error("steering was not queued".into());
                        return;
                    }
                    let middle_key = get(&middle, "prompt_cache_key");
                    c.write(&response("response.completed", "first", &first_key)).await;
                    c.write(&response("response.created", "middle", &middle_key)).await;
                    c.write(&response("response.completed", "middle", &middle_key)).await;
                }
                let p = c.read().await;
                if get(&p, "type") != "response.steer" {
                    c.error(format!("expected steering: {p}"));
                    return;
                }
                c.write(r#"{"type":"response.steer.accepted","steer":{"id":"s1","previous_response_id":"first"}}"#)
                    .await;
                if scenario == "multiple_steers" {
                    let p = c.read().await;
                    if get(&p, "type") != "response.steer" {
                        c.error(format!("expected second steering: {p}"));
                        return;
                    }
                    c.write(r#"{"type":"response.steer.accepted","steer":{"id":"s2","previous_response_id":"first"}}"#)
                        .await;
                    c.write(r#"{"type":"response.steer.failed","steer":{"id":"s2","previous_response_id":"first","input":"second"},"error":{"code":"invalid_input","message":"second failed"}}"#).await;
                }
                if tokio::time::timeout(Duration::from_secs(3), queued.notified())
                    .await
                    .is_err()
                {
                    c.error("create was not queued".into());
                    return;
                }
                let boundary = if scenario == "incomplete" {
                    "response.incomplete"
                } else {
                    "response.completed"
                };
                if scenario != "create_before_steer" {
                    c.write(&response(boundary, "first", &first_key)).await;
                }
                match scenario {
                    "early_tool_result" => c.write(r#"{"type":"response.steer.pending","steer":{"id":"s1","previous_response_id":"first"},"reason":"waiting_for_required_input","required_input":[{"type":"function_call_output","call_id":"c1"}]}"#).await,
                    "failed_steering" => c.write(r#"{"type":"response.steer.failed","steer":{"id":"s1","previous_response_id":"first","input":"first"},"error":{"code":"successor_creation_failed","message":"failed"}}"#).await,
                    _ => {
                        c.write(&response("response.created", "automatic", &first_key)).await;
                        c.write(&response("response.completed", "automatic", &first_key)).await;
                    }
                }
                let next = c.read().await;
                if get(&next, "type") != "response.create" {
                    c.error(format!("expected explicit create: {next}"));
                    return;
                }
                let next_key = get(&next, "prompt_cache_key");
                c.write(&response("response.created", "explicit", &next_key)).await;
                c.write(&response("response.completed", "explicit", &next_key)).await;
                c.idle().await;
            }
        })
        .await;
        let client = Client::new(&format!("successor-{scenario}"));
        let request = |key: &str| {
            format!(
                r#"{{"type":"response.create","model":"gpt-6-astra","prompt_cache_key":"{key}","previous_response_id":"first","input":[]}}"#
            )
        };
        // Go's executor test leaves the execution session out of `req.Metadata`, so its
        // Codex-source key survives; through the conductor Go replaces it with the session
        // UUID, as this executor does. Responses clients keep their key in both.
        let mut stream = client
            .start(&up.url, &request("first-key"), Format::OpenAIResponse)
            .await;
        let (mut queued_create, mut automatic, mut explicit) = (false, false, false);
        while let Some(chunk) = next(&mut stream).await {
            let chunk = chunk.unwrap_or_else(|e| panic!("{scenario}: {e}"));
            let (event, id) = (get(&chunk, "type"), get(&chunk, "response.id"));
            if event == "response.created" && id == "first" {
                if scenario == "create_before_steer" {
                    client.send(&request("middle-key")).await;
                }
                client
                    .send(r#"{"type":"response.steer","previous_response_id":"first","input":"first"}"#)
                    .await;
                if scenario == "create_before_steer" {
                    before_steer.notify_one();
                }
            }
            if event == "response.steer.accepted" {
                if scenario == "multiple_steers" && get(&chunk, "steer.id") == "s1" {
                    client
                        .send(r#"{"type":"response.steer","previous_response_id":"first","input":"second"}"#)
                        .await;
                } else if !queued_create {
                    queued_create = true;
                    client.send(&request("explicit-key")).await;
                    queued.notify_one();
                }
            }
            if event == "response.created" || event == "response.completed" {
                let want = match id.as_str() {
                    "explicit" => "explicit-key",
                    "middle" => "middle-key",
                    _ => "first-key",
                };
                assert_eq!(
                    get(&chunk, "response.prompt_cache_key"),
                    want,
                    "{scenario}: {event} {id} settings"
                );
                automatic |= id == "automatic" && event == "response.completed";
                if id == "explicit" && event == "response.completed" {
                    explicit = true;
                    break;
                }
            }
        }
        drop(stream);
        let want_auto = scenario != "early_tool_result" && scenario != "failed_steering";
        assert!(
            explicit && automatic == want_auto,
            "{scenario}: automatic={automatic} want={want_auto} explicit={explicit}"
        );
        up.finish().await;
    }
}

// ---------------------------------------------------------- codex_websockets_duplex_rejection_test.go

/// `TestCodexDuplexRejectedCreateMetadata`: the upstream waits for every explicit create
/// before rejecting one, so queue ownership is deterministic.
#[tokio::test]
async fn rejected_create_metadata() {
    for (name, active_failure, ambiguous) in [
        ("rejected_create", false, false),
        ("active_failure", true, false),
        ("ambiguous_failure", true, true),
    ] {
        let up = Scripted::start(move |mut c: Peer| async move {
            let event = |kind: &str, id: &str, key: &str| {
                format!(
                    r#"{{"type":"{kind}","response":{{"id":"{id}","prompt_cache_key":"{key}","output":[],"error":{{"type":"invalid_request_error","message":"rejected"}}}}}}"#
                )
            };
            let first_key = get(&c.read().await, "prompt_cache_key");
            c.write(&event("response.created", "first", &first_key)).await;
            let (mut rejected_key, mut rejected_id) = (first_key.clone(), "first");
            if !active_failure {
                c.write(&event("response.completed", "first", &first_key)).await;
                rejected_key = get(&c.read().await, "prompt_cache_key");
                rejected_id = "rejected";
            }
            let good_key = get(&c.read().await, "prompt_cache_key");
            if ambiguous {
                c.write(&event("response.failed", "", &rejected_key)).await;
                c.idle().await;
                return;
            }
            c.write(&event("response.failed", rejected_id, &rejected_key)).await;
            c.write(&event("response.created", "good", &good_key)).await;
            c.write(&event("response.completed", "good", &good_key)).await;
            c.idle().await;
        })
        .await;
        let client = Client::new(&format!("metadata-{name}"));
        let request = |key: &str| {
            format!(r#"{{"type":"response.create","model":"gpt-6-astra","prompt_cache_key":"{key}","input":[]}}"#)
        };
        // Responses source: see `automatic_successor_metadata`.
        let mut stream = client
            .start(&up.url, &request("first-key"), Format::OpenAIResponse)
            .await;
        let (mut sent, mut failed, mut completed, mut terminated) = (false, false, false, false);
        while let Some(chunk) = next(&mut stream).await {
            let chunk = match chunk {
                Ok(chunk) => chunk,
                Err(error) => {
                    assert!(ambiguous && error.scope == FailureScope::Request, "{name}: {error}");
                    terminated = true;
                    continue;
                }
            };
            let (kind, id) = (get(&chunk, "type"), get(&chunk, "response.id"));
            if !sent
                && id == "first"
                && ((active_failure && kind == "response.created") || (!active_failure && kind == "response.completed"))
            {
                sent = true;
                if !active_failure {
                    client.send(&request("rejected-key")).await;
                }
                client.send(&request("good-key")).await;
            }
            if kind == "response.failed" {
                failed = true;
                if ambiguous {
                    continue;
                }
                let want = if active_failure { "first-key" } else { "rejected-key" };
                assert_eq!(
                    get(&chunk, "response.prompt_cache_key"),
                    want,
                    "{name}: failure metadata"
                );
            }
            if kind == "response.completed" && id == "good" {
                completed = true;
                assert_eq!(
                    get(&chunk, "response.prompt_cache_key"),
                    "good-key",
                    "{name}: success consumed another create's metadata"
                );
                break;
            }
        }
        drop(stream);
        if ambiguous {
            assert!(
                failed && terminated && !completed,
                "{name}: failed={failed} terminated={terminated} completed={completed}"
            );
        } else {
            assert!(failed && completed, "{name}: failed={failed} completed={completed}");
        }
        up.finish().await;
    }
}

/// Go's `validCodexReasoningEncryptedContentForTestSeed`.
fn encrypted_seed(seed: u8) -> String {
    use base64::Engine as _;
    let mut payload = vec![0u8; 1 + 8 + 16 + 16 + 32];
    payload[0] = 0x80;
    for (i, byte) in payload.iter_mut().enumerate().skip(9) {
        *byte = seed.wrapping_add(i as u8);
    }
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(payload)
}

/// `TestCodexDuplexLaterInvalidSignatureClearsReplay`: a later invalid-signature
/// rejection clears the rejected request's replay scope only, before the client resends.
#[tokio::test]
async fn later_invalid_signature_clears_replay() {
    for kind in ["response.failed", "error", "error_without_status"] {
        for started in [false, true] {
            let encrypted = encrypted_seed(51);
            let seen = encrypted.clone();
            let up = Scripted::start(move |mut c: Peer| {
                let encrypted = seen.clone();
                async move {
                    c.read().await;
                    c.write(r#"{"type":"response.created","response":{"id":"first","output":[]}}"#).await;
                    c.write(r#"{"type":"response.completed","response":{"id":"first","output":[]}}"#).await;
                    c.read().await;
                    if started {
                        c.write(r#"{"type":"response.created","response":{"id":"rejected","output":[]}}"#).await;
                    }
                    c.write(match kind {
                        "response.failed" => r#"{"type":"response.failed","response":{"id":"rejected","error":{"type":"invalid_request_error","message":"Invalid signature in thinking block"}}}"#,
                        "error" => r#"{"type":"error","status":400,"body":{"error":{"type":"invalid_request_error","message":"Invalid signature in thinking block"}}}"#,
                        _ => r#"{"type":"error","error":{"type":"invalid_request_error","message":"Invalid signature in thinking block"}}"#,
                    })
                    .await;
                    let corrected = c.read().await;
                    if corrected.contains(&encrypted) {
                        c.error("corrected create resent rejected encrypted reasoning".into());
                    }
                    c.write(r#"{"type":"response.created","response":{"id":"corrected","output":[]}}"#).await;
                    c.write(r#"{"type":"response.completed","response":{"id":"corrected","output":[]}}"#).await;
                    c.idle().await;
                }
            })
            .await;
            let client = Client::new(&format!("replay-{kind}-{started}"));
            let request = |session: &str| {
                serde_json::json!({
                    "type": "response.create",
                    "model": MODEL,
                    "metadata": {"user_id": serde_json::json!({"session_id": session}).to_string()},
                    "messages": [{"role": "user", "content": "continue"}],
                })
                .to_string()
            };
            let (first, rejected) = (request("first"), request("rejected"));
            let cache = client.executor.replay.clone();
            let item = format!(r#"{{"type":"reasoning","summary":[],"encrypted_content":"{encrypted}"}}"#);
            for session in ["first", "rejected", "unrelated"] {
                let scope = format!("claude:{session}:agent:main");
                assert!(
                    cache.append(MODEL, &scope, &[item.clone().into_bytes()]),
                    "cache seed failed"
                );
            }
            let mut stream = client.start(&up.url, &first, Format::Claude).await;
            let (mut failed, mut completed) = (false, false);
            while let Some(chunk) = next(&mut stream).await {
                let chunk = chunk.unwrap_or_else(|e| panic!("{kind}/{started}: {e}"));
                let (event, id) = (get(&chunk, "type"), get(&chunk, "response.id"));
                if event == "response.completed" && id == "first" {
                    client.send(&rejected).await;
                }
                if event == "response.failed" || event == "error" {
                    failed = true;
                    assert!(
                        cache.get(MODEL, "claude:rejected:agent:main").is_none(),
                        "{kind}/{started}: rejected scope retained invalid reasoning"
                    );
                    for session in ["first", "unrelated"] {
                        assert!(
                            cache.get(MODEL, &format!("claude:{session}:agent:main")).is_some(),
                            "{kind}/{started}: cleared unrelated scope {session}"
                        );
                    }
                    client.send(&rejected).await;
                }
                if event == "response.completed" && id == "corrected" {
                    completed = true;
                    break;
                }
            }
            drop(stream);
            assert!(
                failed && completed,
                "{kind}/{started}: failed={failed} corrected={completed}"
            );
            up.finish().await;
        }
    }
}

// ----------------------------------------------------- codex_websockets_duplex_initial_failure_test.go

/// The four upstream rejections of `TestCodexDuplexInitialFailure`: (name, payload,
/// status, quota, headers).
const INITIAL_FAILURES: [(&str, &str, u16, bool, bool); 4] = [
    (
        "response_failed_auth",
        r#"{"type":"response.failed","response":{"error":{"type":"authentication_error","message":"expired credential"}}}"#,
        401,
        false,
        false,
    ),
    (
        "response_failed_quota",
        r#"{"type":"response.failed","response":{"error":{"type":"usage_limit_reached","message":"quota exhausted","resets_in_seconds":3600}}}"#,
        429,
        true,
        false,
    ),
    // The top-level status is authoritative even when the error type is generic.
    (
        "error_auth",
        r#"{"type":"error","status":401,"headers":{"X-Request-Id":"initial-rejection"},"error":{"type":"server_error","message":"expired credential"}}"#,
        401,
        false,
        true,
    ),
    (
        "error_quota",
        r#"{"type":"error","status_code":429,"headers":{"X-Request-Id":"initial-rejection"},"error":{"type":"usage_limit_reached","message":"quota exhausted","resets_in_seconds":3600}}"#,
        429,
        true,
        true,
    ),
];

/// `TestCodexDuplexInitialFailure` without failover: the rejection is the stream's only
/// item, an account error with its status, quota scope, retry delay and headers.
#[tokio::test]
async fn initial_failure() {
    for (name, payload, status, quota, headers) in INITIAL_FAILURES {
        let up = Scripted::start(move |mut c: Peer| async move {
            c.read().await;
            c.write(payload).await;
            // Keep the rejected socket open: the executor must close it itself.
            c.idle().await;
        })
        .await;
        let client = Client::new(&format!("initial-{name}"));
        let mut stream = client
            .start(
                &up.url,
                r#"{"model":"duplex-initial-failure-model","input":[]}"#,
                Format::Codex,
            )
            .await;
        let error = next(&mut stream)
            .await
            .expect("an error chunk")
            .expect_err("initial rejection must be an error chunk");
        assert_eq!(error.status, status, "{name}: status lost: {error}");
        assert_ne!(
            error.scope,
            FailureScope::Request,
            "{name}: upstream rejection became a connection-only error"
        );
        if quota {
            assert_eq!(
                error.scope,
                FailureScope::Credential,
                "{name}: credential quota scope lost"
            );
            assert_eq!(
                error.retry_after,
                Some(Duration::from_secs(3600)),
                "{name}: upstream retry delay lost"
            );
        }
        if headers {
            assert_eq!(
                error.headers.get("x-request-id").and_then(|v| v.to_str().ok()),
                Some("initial-rejection"),
                "{name}: upstream error headers lost"
            );
        }
        assert!(
            next(&mut stream).await.is_none(),
            "{name}: rejected stream remained open"
        );
        up.finish().await;
        assert_eq!(up.dials.load(Ordering::SeqCst), 1, "{name}: attempts");
    }
}

// ------------------------------------------------- codex_websockets_duplex_bootstrap_input_test.go

/// `TestCodexDuplexBootstrapCancellationPreservesInput`: cancelling before
/// `response.created` leaves the queued follow-up unread and closes the socket.
#[tokio::test]
async fn bootstrap_cancellation_preserves_input() {
    let initial_read = Arc::new(tokio::sync::Notify::new());
    let read = initial_read.clone();
    let up = Scripted::start(move |mut c: Peer| {
        let read = read.clone();
        async move {
            c.read().await;
            read.notify_one();
            // No response.created: cancellation must release the blocked writer.
            c.idle().await;
        }
    })
    .await;
    let client = Client::new("bootstrap-cancel");
    client
        .input
        .send(Bytes::from_static(br#"{"type":"response.steer","input":[]}"#))
        .await
        .unwrap();
    let stream = client
        .start(
            &up.url,
            r#"{"model":"bootstrap-cancel-model","input":[]}"#,
            Format::Codex,
        )
        .await;
    tokio::time::timeout(Duration::from_secs(5), initial_read.notified())
        .await
        .expect("initial request not sent");
    // Go cancels the context; here the consumer drops the stream.
    drop(stream);
    up.finish().await;
    let mut frames = tokio::time::timeout(Duration::from_secs(3), client.frames.clone().lock_owned())
        .await
        .expect("the cancelled duplex released the client queue");
    assert!(frames.try_recv().is_ok(), "follow-up consumed before response.created");
}

// ------------------------------------------------------- codex_websockets_duplex_health_test.go

/// `TestCodexDuplexConnectionTimeoutDoesNotCoolHealthyAccount`, executor half: the
/// client's input failing after a completed response ends the stream with a connection
/// error, never an account error. (Go injects a read timeout through its input; the
/// queue here only closes, which is what Go's own downstream reader does on any error.)
/// The conductor half is cpa-server's `duplex_dispatch.rs`.
#[tokio::test]
async fn input_failure_is_a_connection_error() {
    let up = Scripted::start(|mut c: Peer| async move {
        c.read().await;
        c.write(r#"{"type":"response.created","response":{"id":"r1"}}"#).await;
        c.write(r#"{"type":"response.completed","response":{"id":"r1","output":[]}}"#)
            .await;
        c.idle().await;
    })
    .await;
    let mut client = Client::new("duplex-health-session");
    let mut stream = client
        .start(&up.url, r#"{"model":"duplex-health-model","input":[]}"#, Format::Codex)
        .await;
    let (mut completed, mut error) = (false, None);
    while let Some(chunk) = next(&mut stream).await {
        match chunk {
            Ok(chunk) if get(&chunk, "type") == "response.completed" => {
                completed = true;
                // The downstream reader is gone: the queue closes.
                let (closed, _) = tokio::sync::mpsc::channel(1);
                drop(std::mem::replace(&mut client.input, closed));
            }
            Ok(_) => {}
            Err(e) => error = Some(e),
        }
    }
    let error = error.expect("the stream ends with an error");
    assert!(completed);
    assert_eq!(error.scope, FailureScope::Request, "{error}");
    up.finish().await;
}

/// Upstream capture in the duplex (`codex_websockets_duplex.go`): each frame the writer
/// sends is `api.websocket.request` with only the URL, method, body, provider and
/// account ID; each upstream message is recorded as received.
#[tokio::test]
async fn capture_records_duplex_frames() {
    use crate::codex_testkit::{Tap, Wiretap};
    const STEER: &str = r#"{"type":"response.steer","previous_response_id":"r1","input":"more"}"#;
    const ACCEPTED: &str = r#"{"type":"response.steer.accepted","steer":{"id":"s1","previous_response_id":"r1"}}"#;
    const COMPLETED: &str = r#"{"type":"response.completed","response":{"id":"r1","output":[]}}"#;
    let up = Scripted::start(|mut c: Peer| async move {
        c.read().await;
        c.write(r#"{"type":"response.created","response":{"id":"r1","output":[]}}"#)
            .await;
        c.read().await;
        c.write(ACCEPTED).await;
        c.write(COMPLETED).await;
        c.idle().await;
    })
    .await;
    let client = Client::new("capture-duplex");
    let tap = Arc::new(Wiretap::default());
    let usage = cpa_core::exec::UsageSink::default().with_capture(cpa_core::exec::CaptureSink::new(tap.clone()));
    let mut stream = client
        .start_with(&up.url, r#"{"model":"gpt-6-astra","input":[]}"#, Format::Codex, usage)
        .await;
    while let Some(chunk) = next(&mut stream).await {
        let chunk = chunk.unwrap();
        match get(&chunk, "type").as_str() {
            "response.created" => client.send(STEER).await,
            "response.completed" => break,
            _ => {}
        }
    }
    drop(stream);
    up.finish().await;
    let taps = tap.taps();
    let credential = credential(&up.url);
    let ws_url = format!("{}/responses", up.url.replacen("http://", "ws://", 1));
    let frame = Tap::WsRequest {
        url: ws_url,
        headers: vec![],
        body: STEER.into(),
        account: ["codex", credential.id.as_str(), "", "", ""].map(str::to_owned),
    };
    assert!(taps.contains(&frame), "{taps:#?}");
    let responses: Vec<&Tap> = taps.iter().filter(|t| matches!(t, Tap::WsResponse(_))).collect();
    assert_eq!(responses.len(), 3, "created, accepted and completed: {taps:#?}");
    assert_eq!(*responses[1], Tap::WsResponse(ACCEPTED.into()));
}

/// Go mutates the socket's multi-agent v2 state in its serial writer, right before the
/// create's write (`codex_websockets_duplex.go:210`). A create still queued behind a
/// blocked write must not change how the running response's events are restored.
#[tokio::test]
async fn queued_create_keeps_the_namespace_state_until_written() {
    let up = Scripted::start(|mut c: Peer| async move {
        c.read().await;
        c.write(r#"{"type":"response.created","response":{"id":"r1","output":[]}}"#)
            .await;
        // Stops reading: the large create below fills the socket buffers.
        tokio::time::sleep(Duration::from_secs(10)).await;
    })
    .await;
    let client = Client::new("namespace-state");
    let mut stream = client
        .start(&up.url, r#"{"model":"gpt-6-astra","input":[]}"#, Format::Codex)
        .await;
    let created = next(&mut stream).await.unwrap().unwrap();
    assert_eq!(get(&created, "type"), "response.created");
    let conn = client.executor.ws.session("namespace-state").current().unwrap();
    // An earlier request on this socket renamed the collaboration namespace.
    conn.multi_agent.store(true, Ordering::SeqCst);
    let large = format!(
        r#"{{"type":"response.create","input":[{{"role":"user","content":"{}"}}]}}"#,
        // Twice what loopback socket buffers can hold (4 MiB send plus 6 MiB receive).
        "x".repeat(20 << 20)
    );
    client.send(&large).await;
    // Uses the upstream names itself (`multiAgentV2Conflict`), but waits behind `large`.
    client
        .send(r#"{"type":"response.create","tools":[{"type":"namespace","name":"collaboration-optimize","tools":[]}],"input":[]}"#)
        .await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(
        conn.multi_agent.load(Ordering::SeqCst),
        "a create that was not written yet changed the socket's namespace state"
    );
    drop(stream);
}
