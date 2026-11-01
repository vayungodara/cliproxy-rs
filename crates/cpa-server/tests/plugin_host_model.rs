//! v8.0.35 internal host.model callbacks through the production dispatcher and
//! native plugins, with private credentials and loopback-only upstreams.
#![cfg(unix)]

use std::path::Path;
use std::sync::{Arc, Mutex};

use axum::body::Bytes;
use axum::extract::{Request, State};
use axum::response::{IntoResponse, Response};
use base64::Engine as _;
use cpa_core::config::Config;
use cpa_exec::Executors;
use cpa_plugin::callbacks::RequestScope;
use cpa_plugin::client::CallbackInstance;
use cpa_plugin::testing::{call_from_plugin, callback_context};
use serde_json::{Value, json};

const BODY: &str = r#"{ "model":"fast", "messages":[{"role":"user","content":"hello"}] }"#;
const REPLY: &str = r#"{ "id":"reply", "choices":[{"message":{"content":"ok"}}] }"#;
const CHUNK: &str = r#"{"id":"reply","choices":[{"delta":{"content":"ok"}}]}"#;

/// The tests share process-wide plugin and callback state, so they take turns when they
/// run as threads of one process (plain `cargo test`; nextest runs one per process).
static SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

#[derive(Default)]
struct Seen(Mutex<Vec<(String, String, Bytes)>>);

async fn upstream(State(seen): State<Arc<Seen>>, req: Request) -> Response {
    let path = req.uri().to_string();
    let auth = req.headers()["authorization"].to_str().unwrap().to_owned();
    let body = axum::body::to_bytes(req.into_body(), usize::MAX).await.unwrap();
    let fields: Value = serde_json::from_slice(&body).unwrap();
    let stream = fields["stream"] == true;
    seen.0.lock().unwrap().push((path, auth, body));
    if fields["wait"] == true {
        return std::future::pending().await;
    }
    let mut response = if stream {
        ([("x-upstream", "stream")], format!("data: {CHUNK}\n\ndata: [DONE]\n\n")).into_response()
    } else {
        ([("x-upstream", "buffered")], REPLY).into_response()
    };
    for (name, value) in [
        ("set-cookie", "session=fake"),
        ("connection", "x-private"),
        ("x-private", "hop"),
        ("x-litellm-test", "gateway"),
        ("x-cpa-trace-id", "reserved"),
    ] {
        response.headers_mut().insert(name, value.parse().unwrap());
    }
    response
}

async fn serve(seen: Arc<Seen>) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move {
        axum::serve(listener, axum::Router::new().fallback(upstream).with_state(seen))
            .await
            .unwrap();
    });
    url
}

/// A Codex upstream for API keys with `websockets: true`: each upgrade or HTTP request
/// is recorded as `(kind, authorization)`, and every turn completes at once.
async fn serve_codex(seen: Arc<Mutex<Vec<(&'static str, String)>>>) -> String {
    use axum::extract::ws::{Message, WebSocketUpgrade};
    use axum::response::IntoResponse;
    const DONE: &str = r#"{"type":"response.completed","response":{"id":"r1","status":"completed","output":[],"usage":{"input_tokens":1,"output_tokens":1,"total_tokens":2}}}"#;
    let handler = move |ws: Result<WebSocketUpgrade, axum::extract::ws::rejection::WebSocketUpgradeRejection>,
                        headers: axum::http::HeaderMap| {
        let seen = seen.clone();
        async move {
            let auth = headers["authorization"].to_str().unwrap().to_owned();
            let Ok(ws) = ws else {
                seen.lock().unwrap().push(("http", auth));
                return format!("event: response.completed\ndata: {DONE}\n\n").into_response();
            };
            seen.lock().unwrap().push(("websocket", auth));
            ws.on_upgrade(|mut socket| async move {
                while let Some(Ok(message)) = socket.recv().await {
                    if matches!(message, Message::Text(_)) {
                        let created =
                            r#"{"type":"response.created","response":{"id":"r1","status":"in_progress","output":[]}}"#;
                        for event in [created, DONE] {
                            let _ = socket.send(Message::Text(event.into())).await;
                        }
                    }
                }
            })
        }
    };
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move {
        axum::serve(listener, axum::Router::new().fallback(handler))
            .await
            .unwrap()
    });
    url
}

fn b64(bytes: impl AsRef<[u8]>) -> String {
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

fn decode(value: &Value) -> Vec<u8> {
    base64::engine::general_purpose::STANDARD
        .decode(value.as_str().unwrap())
        .unwrap()
}

fn check_headers(headers: &Value, upstream: &str) {
    assert_eq!(headers["X-Upstream"], json!([upstream]));
    for name in [
        "Set-Cookie",
        "Connection",
        "X-Private",
        "X-Litellm-Test",
        "X-Cpa-Trace-Id",
    ] {
        assert!(headers.get(name).is_none(), "unsafe upstream header {name}");
    }
}

fn canned(records: &Path, label: &str, method: &str, result: Value) {
    let dir = records.join("respond").join(label);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join(format!("{method}.json")),
        json!({"ok":true,"result":result}).to_string(),
    )
    .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn callbacks_execute_pin_and_stream_without_recursing_into_the_caller() {
    let _serial = SERIAL.lock().await;
    let tmp = Path::new(env!("CARGO_TARGET_TMPDIR"));
    let Some(built) = cpa_plugin::testing::built_plugins(tmp, "plugin_host_model") else {
        return;
    };
    let dir = cpa_plugin::testing::scratch(tmp, "host-model");
    let auths = dir.join("auths");
    let records = dir.join("records");
    let plugins = dir.join("plugins");
    for d in [&auths, &records, &plugins] {
        std::fs::create_dir_all(d).unwrap();
    }
    let seen = Arc::new(Seen::default());
    let proxy_seen = Arc::new(Seen::default());
    let url = serve(seen.clone()).await;
    // A mock HTTP proxy answers requests itself; it never forwards traffic.
    let proxy = serve(proxy_seen.clone()).await;
    let yaml = format!(
        "auth-dir: {}\napi-keys:\n  openai-compatibility:\n    - name: Acme\n      base-url: {url}/v1\n      models: [{{name: up-model, alias: fast}}]\n      keys: [{{api-key: first}}, {{api-key: second}}]\n",
        auths.display()
    );
    let cfg = Config::parse(&yaml).unwrap();
    let credentials = cpa_core::config::credentials::load(&cfg);
    let pinned = credentials[1].id.clone();
    let executors = Executors {
        claude: cpa_exec::claude::ClaudeExecutor::new("http://127.0.0.1:1").unwrap(),
        codex: Default::default(),
        devices: Default::default(),
        openai: Default::default(),
        google: Default::default(),
    };
    let rt = Arc::new(cpa_server::testing::runtime(cfg, credentials, executors));
    cpa_server::plugins::start(&rt).await;
    let host = rt.plugins().clone();
    let instance = Arc::new(CallbackInstance::default());
    let guard = callback_context(&host, "origin", instance.clone(), RequestScope::default());
    let id = guard.id().to_owned();
    let (h, i, pin) = (host.clone(), instance.clone(), pinned.clone());
    tokio::task::spawn_blocking(move || {
        let call = |method, request: Value| -> Value {
            let bytes = call_from_plugin(&h, "origin", &i, method, &serde_json::to_vec(&request).unwrap()).unwrap();
            serde_json::from_slice::<Value>(&bytes).unwrap()["result"].clone()
        };
        let request = json!({"entry_protocol":"openai", "model":"fast", "auth_id":pin,
            "body":b64(BODY), "host_callback_id":id});
        let response = call("host.model.execute", request.clone());
        assert_eq!(response["status_code"], 200);
        assert_eq!(decode(&response["body"]), REPLY.as_bytes());
        check_headers(&response["headers"], "buffered");
        let mut overridden = request.clone();
        overridden["proxy_url"] = json!(proxy);
        assert_eq!(
            decode(&call("host.model.execute", overridden)["body"]),
            REPLY.as_bytes()
        );
        let mut stream = request;
        stream["stream"] = json!(true);
        stream["body"] = json!(b64(BODY.replace("\"model\":", "\"stream\":true,\"model\":")));
        let opened = call("host.model.execute_stream", stream);
        check_headers(&opened["headers"], "stream");
        let read = json!({"stream_id":opened["stream_id"]});
        let first = call("host.model.stream_read", read.clone());
        assert_eq!(
            decode(&first["payload"]),
            CHUNK.as_bytes(),
            "internal chunks are not SSE frames"
        );
        loop {
            if call("host.model.stream_read", read.clone())["done"] == true {
                break;
            }
        }
        call("host.model.stream_close", read);
    })
    .await
    .unwrap();
    assert_eq!(seen.0.lock().unwrap().len(), 2);
    assert_eq!(proxy_seen.0.lock().unwrap().len(), 1);
    for (_, auth, body) in seen.0.lock().unwrap().iter().chain(proxy_seen.0.lock().unwrap().iter()) {
        assert_eq!(auth, "Bearer second");
        assert_eq!(serde_json::from_slice::<Value>(body).unwrap()["model"], "up-model");
    }
    assert!(
        rt.store()
            .snapshot()
            .iter()
            .all(|c| !c.attributes.contains_key("proxy_url"))
    );

    for label in ["origin", "other"] {
        std::fs::copy(built.join("recorder.so"), plugins.join(format!("{label}.so"))).unwrap();
        for method in [
            "request.intercept_before",
            "request.intercept_after",
            "response.intercept_after",
            "response.intercept_stream_chunk",
            "request.complete",
        ] {
            canned(&records, label, method, json!({}));
        }
        canned(
            &records,
            label,
            "model.route",
            json!({"handled":true,"target_kind":"provider","target":"openai-compatible-acme"}),
        );
    }
    let mut plugin_yaml = format!(
        "{yaml}plugins:\n  enabled: true\n  dir: {}\n  configs:\n",
        plugins.display()
    );
    for label in ["origin", "other"] {
        plugin_yaml.push_str(&format!(
            "    {label}:\n      enabled: true\n      label: {label}\n      record: {}\n      caps: management_api,model_router,request_interceptor,response_interceptor,response_stream_interceptor,request_lifecycle_plugin\n",
            records.display()
        ));
    }
    let cfg = Config::parse(&plugin_yaml).unwrap();
    cpa_server::plugins::sync(&rt, Arc::new(cfg)).await;
    assert!(host.plugin_registered("origin") && host.plugin_registered("other"));
    let nested = json!({"calls":[{"method":"host.model.execute","request":{
        "entry_protocol":"openai","model":"fast","auth_id":pinned,"body":b64(BODY)}}]});
    let response = host
        .serve_management(cpa_plugin::management::Inbound {
            method: "POST".into(),
            path: "/v0/management/rec/origin/calls".into(),
            headers: [("X-Inherited".into(), vec!["outer".into()])].into(),
            query: [("q".into(), vec!["one".into(), "two".into()])].into(),
            body: Bytes::from(serde_json::to_vec(&nested).unwrap()),
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(response.status, 201);
    let response: Value = serde_json::from_slice(&response.body).unwrap();
    let nested = response["results"][0].as_str().unwrap().strip_prefix("0 ").unwrap();
    let nested: Value = serde_json::from_str(nested).unwrap();
    assert_eq!(decode(&nested["result"]["body"]), REPLY.as_bytes());
    let methods = |label: &str| -> Vec<String> {
        cpa_server::testing::records(&records.join(format!("{label}.jsonl")))
            .into_iter()
            .map(|record| record["method"].as_str().unwrap().to_owned())
            .filter(|method| {
                method.starts_with("request.") || method.starts_with("response.") || method == "model.route"
            })
            .collect()
    };
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        while !methods("other").iter().any(|method| method == "request.complete") {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    assert!(methods("origin").is_empty(), "the caller must not recurse into itself");
    assert_eq!(
        methods("other"),
        [
            "model.route",
            "request.intercept_before",
            "request.intercept_after",
            "response.intercept_after",
            "request.complete"
        ]
    );
    let route: Value = cpa_server::testing::records(&records.join("other.jsonl"))
        .into_iter()
        .find(|record| record["method"] == "model.route")
        .unwrap();
    let route: Value = serde_json::from_str(route["request"].as_str().unwrap()).unwrap();
    assert_eq!(route["Headers"]["X-Inherited"], json!(["outer"]));
    assert_eq!(route["Query"]["q"], json!(["one", "two"]));
    let (h, i, id) = (host.clone(), instance, guard.id().to_owned());
    tokio::task::spawn_blocking(move || {
        let call = |method, request: Value| -> Value {
            let raw = call_from_plugin(&h, "origin", &i, method, &serde_json::to_vec(&request).unwrap()).unwrap();
            serde_json::from_slice::<Value>(&raw).unwrap()["result"].clone()
        };
        let opened = call(
            "host.model.execute_stream",
            json!({"entry_protocol":"openai","model":"fast",
            "stream":true,"body":b64(BODY.replace("\"model\":", "\"stream\":true,\"model\":")),"host_callback_id":id}),
        );
        let read = json!({"stream_id":opened["stream_id"]});
        assert_eq!(
            decode(&call("host.model.stream_read", read.clone())["payload"]),
            CHUNK.as_bytes()
        );
        while call("host.model.stream_read", read.clone())["done"] != true {}
    })
    .await
    .unwrap();
    let methods = |label: &str| -> Vec<String> {
        cpa_server::testing::records(&dir.join("records").join(format!("{label}.jsonl")))
            .into_iter()
            .map(|record| record["method"].as_str().unwrap().to_owned())
            .collect()
    };
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        while methods("other")
            .iter()
            .filter(|method| *method == "request.complete")
            .count()
            != 2
        {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    assert!(
        methods("other")
            .iter()
            .any(|method| method == "response.intercept_stream_chunk")
    );
    assert!(
        !methods("origin")
            .iter()
            .any(|method| method.starts_with("request.") || method.starts_with("response.") || method == "model.route")
    );
    let count = seen.0.lock().unwrap().len();
    let h = host.clone();
    let blocked = tokio::spawn(async move {
        let request = json!({"calls":[{"method":"host.model.execute","request":{
            "entry_protocol":"openai","model":"fast","body":b64(BODY.replace("\"model\":", "\"wait\":true,\"model\":"))}}]});
        h.serve_management(cpa_plugin::management::Inbound {
            method: "POST".into(),
            path: "/v0/management/rec/origin/calls".into(),
            body: Bytes::from(serde_json::to_vec(&request).unwrap()),
            ..Default::default()
        })
        .await
    });
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        while seen.0.lock().unwrap().len() == count {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    blocked.abort();
    assert!(blocked.await.unwrap_err().is_cancelled());
    // Shutdown waits for active native calls, so this also proves cancellation
    // releases the plugin blocked in its synchronous host.model callback.
    tokio::time::timeout(std::time::Duration::from_secs(2), host.shutdown_all(None))
        .await
        .unwrap();
    drop(guard);
    std::fs::remove_dir_all(dir).unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn callback_streams_preserve_executor_bytes_across_interceptor_unload() {
    let _serial = SERIAL.lock().await;
    let tmp = Path::new(env!("CARGO_TARGET_TMPDIR"));
    let Some(built) = cpa_plugin::testing::built_plugins(tmp, "plugin_host_model_bytes") else {
        return;
    };
    let dir = cpa_plugin::testing::scratch(tmp, "host-model-bytes");
    let records = dir.join("records");
    let plugins = dir.join("plugins");
    let auths = dir.join("auths");
    for d in [&records, &plugins, &auths] {
        std::fs::create_dir_all(d).unwrap();
    }
    std::fs::copy(built.join("recorder.so"), plugins.join("observer.so")).unwrap();
    canned(&records, "observer", "response.intercept_stream_chunk", json!({}));
    // Expected chunks are Go v8.0.35 executor output, not HTTP handler output.
    let fixtures: [(&str, &str, &str, &str, Vec<&str>); 4] = [
        (
            "openai",
            "openai-compatibility",
            "fast",
            "data: {\ndata: \"choices\":[]\ndata: }\n\ndata: {\ndata: \"choices\":[1]\ndata: }\n\ndata: [DONE]\n\n",
            vec!["{\n\"choices\":[]\n}", "{\n\"choices\":[1]\n}"],
        ),
        (
            "gemini",
            "gemini",
            "gemini-2.5-flash",
            "data: {\"candidates\":[]}\n\ndata: {\"candidates\":[{\"finishReason\":\"STOP\"}]}\n\n",
            vec![r#"{"candidates":[]}"#, r#"{"candidates":[{"finishReason":"STOP"}]}"#],
        ),
        (
            "claude",
            "claude",
            "claude-sonnet-4-6",
            "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"msg\",\"model\":\"claude-sonnet-4-6\"}}\n\nevent: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
            vec![
                "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"msg\",\"model\":\"claude-sonnet-4-6\"}}\n\n",
                "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
            ],
        ),
        (
            "openai-response",
            "codex",
            "gpt-5.4",
            "event: response.output_text.delta\ndata: {\"type\":\"response.output_text.delta\",\"delta\":\"hi\"}\n\nevent: response.completed\ndata: {\"type\":\"response.completed\",\"response\":{\"id\":\"r\",\"output\":[]}}\n\n",
            vec![
                "event: response.output_text.delta",
                r#"data: {"type":"response.output_text.delta","delta":"hi"}"#,
                "event: response.completed",
                r#"data: {"type":"response.completed","response":{"id":"r","output":[]}}"#,
            ],
        ),
    ];
    for (protocol, family, model, wire, expected) in fixtures {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            axum::serve(
                listener,
                axum::Router::new().fallback(move || async move { ([("content-type", "text/event-stream")], wire) }),
            )
            .await
            .unwrap();
        });
        let config = if family == "openai-compatibility" {
            format!(
                "    - name: Fixture\n      base-url: {url}\n      models: [{{name: fast}}]\n      keys: [{{api-key: fixture}}]\n"
            )
        } else {
            format!("    - base-url: {url}\n      models: [{{name: {model}}}]\n      keys: [{{api-key: fixture}}]\n")
        };
        let yaml = format!("auth-dir: {}\napi-keys:\n  {family}:\n{config}", auths.display());
        let cfg = Config::parse(&yaml).unwrap();
        let credentials = cpa_core::config::credentials::load(&cfg);
        assert_eq!(credentials.len(), 1, "{family}");
        let rt = Arc::new(cpa_server::testing::runtime(
            cfg,
            credentials,
            Executors {
                claude: cpa_exec::claude::ClaudeExecutor::new(&url).unwrap(),
                codex: Default::default(),
                devices: Default::default(),
                openai: Default::default(),
                google: Default::default(),
            },
        ));
        cpa_server::plugins::start(&rt).await;
        let host = rt.plugins().clone();
        for mode in ["absent", "present", "unloaded"] {
            if mode != "absent" {
                let cfg = Config::parse(&format!("{yaml}plugins:\n  enabled: true\n  dir: {}\n  configs:\n    observer:\n      enabled: true\n      label: observer\n      record: {}\n      caps: response_stream_interceptor\n", plugins.display(), records.display())).unwrap();
                cpa_server::plugins::sync(&rt, Arc::new(cfg)).await;
                assert!(host.has_stream_interceptors());
            }
            let instance = Arc::new(CallbackInstance::default());
            let guard = callback_context(&host, "origin", instance.clone(), RequestScope::default());
            let request = json!({"host_callback_id":guard.id(), "entry_protocol":protocol, "model":model, "stream":true,
                "body":b64(json!({"model":model,"stream":true,"messages":[{"role":"user","content":"hi"}],"contents":[{"role":"user","parts":[{"text":"hi"}]}],"input":"hi","max_tokens":16}).to_string())});
            let h = host.clone();
            let i = instance.clone();
            let opened = tokio::task::spawn_blocking(move || callback(&h, &i, "host.model.execute_stream", request))
                .await
                .unwrap();
            let read = json!({"stream_id":opened["stream_id"]});
            let mut got = vec![];
            loop {
                let (h, i, read) = (host.clone(), instance.clone(), read.clone());
                let chunk = tokio::task::spawn_blocking(move || callback(&h, &i, "host.model.stream_read", read))
                    .await
                    .unwrap();
                assert_eq!(chunk["error"], "", "{protocol}/{mode}: {chunk}");
                if chunk["done"] == true {
                    break;
                }
                assert!(chunk["payload"].is_string(), "{protocol}/{mode}: {chunk}; {got:?}");
                got.push(String::from_utf8(decode(&chunk["payload"])).unwrap());
                if mode == "unloaded" && got.len() == 1 {
                    assert!(host.unload_plugin("observer", None).await);
                    assert!(!host.has_stream_interceptors());
                }
            }
            assert_eq!(got, expected, "{protocol}/{mode}");
            if mode != "absent" {
                let bodies: Vec<_> = cpa_server::testing::records(&records.join("observer.jsonl"))
                    .into_iter()
                    .filter(|r| r["method"] == "response.intercept_stream_chunk")
                    .map(|r| serde_json::from_str::<Value>(r["request"].as_str().unwrap()).unwrap())
                    .filter(|r| r["ChunkIndex"].as_i64().unwrap() >= 0)
                    .map(|r| String::from_utf8(decode(&r["Body"])).unwrap())
                    .collect();
                assert_eq!(
                    bodies,
                    if mode == "present" {
                        expected.clone()
                    } else {
                        vec![expected[0]]
                    },
                    "interceptor {protocol}/{mode}"
                );
                std::fs::remove_file(records.join("observer.jsonl")).unwrap();
            }
            drop(guard);
        }
        host.shutdown_all(None).await;
        server.abort();
    }
    std::fs::remove_dir_all(dir).unwrap();
}

fn callback(host: &cpa_plugin::Host, instance: &Arc<CallbackInstance>, method: &str, request: Value) -> Value {
    let bytes = call_from_plugin(host, "origin", instance, method, &serde_json::to_vec(&request).unwrap()).unwrap();
    let envelope: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(envelope["ok"], true, "{method}: {envelope}");
    envelope["result"].clone()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn nested_callbacks_inherit_pin_session_and_path_but_not_proxy() {
    let _serial = SERIAL.lock().await;
    let tmp = Path::new(env!("CARGO_TARGET_TMPDIR"));
    let Some(built) = cpa_plugin::testing::built_plugins(tmp, "plugin_host_model_context") else {
        return;
    };
    let dir = cpa_plugin::testing::scratch(tmp, "host-model-context");
    let auths = dir.join("auths");
    let records = dir.join("records");
    let plugins = dir.join("plugins");
    for d in [&auths, &records, &plugins] {
        std::fs::create_dir_all(d).unwrap();
    }
    let seen = Arc::new(Seen::default());
    let proxy_seen = Arc::new(Seen::default());
    let url = serve(seen.clone()).await;
    let proxy = serve(proxy_seen.clone()).await;
    let codex_seen = Arc::new(Mutex::new(Vec::new()));
    let codex = serve_codex(codex_seen.clone()).await;
    let yaml = format!(
        "auth-dir: {}\napi-keys:\n  openai-compatibility:\n    - name: Acme\n      base-url: {url}/v1\n      models: [{{name: up-model, alias: fast}}]\n      keys: [{{api-key: first}}, {{api-key: second}}]\n  codex:\n    - base-url: {codex}\n      models: [{{name: gpt-fixture}}]\n      keys: [{{api-key: ws-one, websockets: true}}, {{api-key: ws-two, websockets: true}}]\nplugins:\n  enabled: true\n  dir: {}\n  configs:\n",
        auths.display(),
        plugins.display()
    );
    let mut yaml = yaml;
    for label in ["origin", "nested"] {
        std::fs::copy(built.join("recorder.so"), plugins.join(format!("{label}.so"))).unwrap();
        yaml.push_str(&format!("    {label}:\n      enabled: true\n      label: {label}\n      record: {}\n      caps: request_interceptor\n", records.display()));
        for method in ["request.intercept_before", "request.intercept_after"] {
            canned(&records, label, method, json!({}));
        }
    }
    let cfg = Config::parse(&yaml).unwrap();
    let credentials = cpa_core::config::credentials::load(&cfg);
    let acme: Vec<_> = credentials.iter().filter(|c| c.provider != "codex").collect();
    let first = acme[0].id.clone();
    let pinned = acme[1].id.clone();
    let by_key: std::collections::HashMap<String, String> = credentials
        .iter()
        .filter_map(|c| Some((format!("Bearer {}", c.attributes.get("api_key")?), c.id.clone())))
        .collect();
    let rt = Arc::new(cpa_server::testing::runtime(
        cfg,
        credentials,
        Executors {
            claude: cpa_exec::claude::ClaudeExecutor::new("http://127.0.0.1:1").unwrap(),
            codex: cpa_exec::codex::CodexExecutor::new().unwrap(),
            devices: Default::default(),
            openai: Default::default(),
            google: Default::default(),
        },
    ));
    cpa_server::plugins::start(&rt).await;
    let host = rt.plugins().clone();
    let instance = Arc::new(CallbackInstance::default());
    let guard = callback_context(&host, "origin", instance.clone(), RequestScope::default());
    for override_pin in [None, Some(first.as_str())] {
        let mut nested = json!({"entry_protocol":"openai","model":"fast","body":b64(BODY)});
        if let Some(id) = override_pin {
            nested["auth_id"] = json!(id);
        }
        std::fs::write(
            records.join("respond/nested/request.intercept_before.json"),
            json!({"ok":true,"result":{},"host_calls":[{"method":"host.model.execute","request":nested}]}).to_string(),
        )
        .unwrap();
        let (h, i) = (host.clone(), instance.clone());
        let request = json!({"host_callback_id":guard.id(),"entry_protocol":"openai","model":"fast","body":b64(BODY),
            "auth_id":pinned,"proxy_url":proxy,"path":"/inherited/path"});
        let response = tokio::task::spawn_blocking(move || callback(&h, &i, "host.model.execute", request))
            .await
            .unwrap();
        assert_eq!(decode(&response["body"]), REPLY.as_bytes());
        // Outer callback uses its proxy; the nested callback must clear that override.
        assert_eq!(proxy_seen.0.lock().unwrap().last().unwrap().1, "Bearer second");
        assert_eq!(
            seen.0.lock().unwrap().last().unwrap().1,
            if override_pin.is_some() {
                "Bearer first"
            } else {
                "Bearer second"
            }
        );
    }
    // A real Responses WebSocket: its first turn pins the connection to the credential
    // that served it, and a callback entered from the second turn's interceptor must
    // inherit that pin, the connection's execution session and its request path.
    let proxy_addr = {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let app = cpa_server::router(rt.clone());
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        addr
    };
    let mut socket = wreq::Client::new()
        .websocket(format!("ws://{proxy_addr}/v1/responses"))
        .send()
        .await
        .unwrap()
        .into_websocket()
        .await
        .unwrap();
    async fn turn(socket: &mut wreq::ws::WebSocket) {
        use futures_util::StreamExt as _;
        use wreq::ws::message::Message;
        socket
            .send(Message::text(
                r#"{"type":"response.create","model":"gpt-fixture","input":[{"type":"message","role":"user","content":[{"type":"input_text","text":"hi"}]}]}"#,
            ))
            .await
            .unwrap();
        loop {
            let frame = tokio::time::timeout(std::time::Duration::from_secs(10), socket.next())
                .await
                .expect("turn completes")
                .expect("socket stays open")
                .unwrap();
            if let Message::Text(text) = frame {
                assert!(!text.as_str().contains("\"type\":\"error\""), "{}", text.as_str());
                if text.as_str().contains("response.completed") {
                    return;
                }
            }
        }
    }
    canned(&records, "nested", "request.intercept_before", json!({}));
    turn(&mut socket).await;
    let served = codex_seen.lock().unwrap()[0].clone();
    assert_eq!(served.0, "websocket");
    let pinned = &by_key[&served.1];
    std::fs::write(
        records.join("respond/nested/request.intercept_before.json"),
        json!({"ok":true,"result":{},"host_calls":[{"method":"host.model.execute","request":{
            "entry_protocol":"openai-response","model":"gpt-fixture","body":b64(r#"{"model":"gpt-fixture","input":"nested"}"#)}}]})
        .to_string(),
    )
    .unwrap();
    std::fs::remove_file(records.join("origin.jsonl")).unwrap();
    let before = codex_seen.lock().unwrap().len();
    turn(&mut socket).await;
    let requests: Vec<Value> = cpa_server::testing::records(&records.join("origin.jsonl"))
        .into_iter()
        .filter(|r| r["method"] == "request.intercept_before")
        .map(|r| serde_json::from_str::<Value>(r["request"].as_str().unwrap()).unwrap())
        .collect();
    assert_eq!(requests.len(), 2, "pinned WebSocket turn and nested callback");
    let session = requests[0]["Metadata"]["execution_session_id"].clone();
    assert!(session.as_str().is_some_and(|s| !s.is_empty()), "{session}");
    for request in &requests {
        assert_eq!(request["Metadata"]["pinned_auth_id"], *pinned);
        assert_eq!(request["Metadata"]["execution_session_id"], session);
        assert_eq!(request["Metadata"]["request_path"], "/v1/responses");
    }
    // The nested call goes out on the pinned credential; the outer turn reuses its socket.
    let turn_two = codex_seen.lock().unwrap()[before..].to_vec();
    assert!(!turn_two.is_empty());
    assert!(turn_two.iter().all(|(_, auth)| *auth == served.1), "{turn_two:?}");
    let results: Vec<Value> = cpa_server::testing::records(&records.join("nested.jsonl"))
        .into_iter()
        .filter(|r| r["method"] == "host_callback")
        .map(|r| serde_json::from_str(r["request"].as_str().unwrap()).unwrap())
        .collect();
    assert_eq!(results.len(), 3, "two HTTP callbacks and one from the WebSocket turn");
    for (i, response) in results.iter().enumerate() {
        assert_eq!(response["ok"], true, "{response}");
        let body = decode(&response["result"]["body"]);
        if i < 2 {
            assert_eq!(body, REPLY.as_bytes());
        } else {
            let body: Value = serde_json::from_slice(&body).unwrap();
            assert_eq!(body["status"], "completed", "{body}");
        }
    }
    host.shutdown_all(None).await;
    drop(guard);
    std::fs::remove_dir_all(dir).unwrap();
}

/// Go restores the parent callback context for `host.model.execute`, so the nested
/// call's usage record carries the client key and access provider of the request whose
/// interceptor made it (37fe2608), like the request's own record. That includes the
/// response interceptors of the unified and Gemini model lists, which keep the request
/// context (Go `WriteModelListResponse`).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn nested_callbacks_record_the_parent_usage_identity() {
    let _serial = SERIAL.lock().await;
    let tmp = Path::new(env!("CARGO_TARGET_TMPDIR"));
    let Some(built) = cpa_plugin::testing::built_plugins(tmp, "plugin_host_model_usage") else {
        return;
    };
    let dir = cpa_plugin::testing::scratch(tmp, "host-model-usage");
    let auths = dir.join("auths");
    let records = dir.join("records");
    let plugins = dir.join("plugins");
    for d in [&auths, &records, &plugins] {
        std::fs::create_dir_all(d).unwrap();
    }
    let seen = Arc::new(Seen::default());
    let url = serve(seen.clone()).await;
    std::fs::copy(built.join("recorder.so"), plugins.join("nested.so")).unwrap();
    let yaml = format!(
        "auth-dir: {}\naccess:\n  api-keys: [k1]\napi-keys:\n  openai-compatibility:\n    - name: Acme\n      base-url: {url}/v1\n      models: [{{name: up-model, alias: fast}}]\n      keys: [{{api-key: first}}]\nobservability: {{usage: {{usage-statistics-enabled: true}}}}\nplugins:\n  enabled: true\n  dir: {}\n  configs:\n    nested:\n      enabled: true\n      label: nested\n      record: {}\n      caps: request_interceptor,response_interceptor\n",
        auths.display(),
        plugins.display(),
        records.display()
    );
    canned(&records, "nested", "request.intercept_after", json!({}));
    canned(&records, "nested", "response.intercept_after", json!({}));
    std::fs::write(
        records.join("respond/nested/request.intercept_before.json"),
        json!({"ok":true,"result":{},"host_calls":[{"method":"host.model.execute",
            "request":{"entry_protocol":"openai","model":"fast","body":b64(BODY)}}]})
        .to_string(),
    )
    .unwrap();
    let cfg = Config::parse(&yaml).unwrap();
    let credentials = cpa_core::config::credentials::load(&cfg);
    let rt = Arc::new(cpa_server::testing::runtime(
        cfg.clone(),
        credentials,
        Executors {
            claude: cpa_exec::claude::ClaudeExecutor::new("http://127.0.0.1:1").unwrap(),
            codex: cpa_exec::codex::CodexExecutor::new().unwrap(),
            devices: Default::default(),
            openai: Default::default(),
            google: Default::default(),
        },
    ));
    rt.usage_queue().configure(true, &cfg);
    cpa_server::plugins::start(&rt).await;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let app = cpa_server::router(rt.clone());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let response = wreq::Client::new()
        .post(format!("http://{addr}/v1/chat/completions"))
        .header("authorization", "Bearer k1")
        .header("content-type", "application/json")
        .body(BODY)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status().as_u16(), 200);
    assert_eq!(seen.0.lock().unwrap().len(), 2, "the nested call and the request");
    let identities: Vec<(Value, Value, Value)> = rt
        .usage_queue()
        .pop_oldest(10)
        .iter()
        .map(|r| serde_json::from_slice::<Value>(r).unwrap())
        .map(|r| {
            (
                r["api_key"].clone(),
                r["is_native_key"].clone(),
                r["access_provider"].clone(),
            )
        })
        .collect();
    let want = (json!("k1"), json!(true), json!("config-inline"));
    assert_eq!(identities, [want.clone(), want.clone()]);

    canned(&records, "nested", "request.intercept_before", json!({}));
    std::fs::write(
        records.join("respond/nested/response.intercept_after.json"),
        json!({"ok":true,"result":{},"host_calls":[{"method":"host.model.execute",
            "request":{"entry_protocol":"openai","model":"fast","body":b64(BODY)}}]})
        .to_string(),
    )
    .unwrap();
    for path in ["/v1/models", "/v1beta/models"] {
        let response = wreq::Client::new()
            .get(format!("http://{addr}{path}"))
            .header("authorization", "Bearer k1")
            .send()
            .await
            .unwrap();
        assert_eq!(response.status().as_u16(), 200, "{path}");
    }
    assert_eq!(seen.0.lock().unwrap().len(), 4, "one nested call per model list");
    let identities: Vec<(Value, Value, Value)> = rt
        .usage_queue()
        .pop_oldest(10)
        .iter()
        .map(|r| serde_json::from_slice::<Value>(r).unwrap())
        .map(|r| {
            (
                r["api_key"].clone(),
                r["is_native_key"].clone(),
                r["access_provider"].clone(),
            )
        })
        .collect();
    assert_eq!(identities, [want.clone(), want]);
    rt.plugins().shutdown_all(None).await;
    std::fs::remove_dir_all(dir).unwrap();
}

/// Go openai_responses_handlers.go (bootstrap EOF and error branches): a data frame still
/// pending in the Responses framer is flushed and delivered before the bootstrap decides
/// between SSE and an initial JSON error, whether or not a plugin hook made the executor
/// hand over unframed payloads.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn responses_bootstrap_flushes_a_pending_payload() {
    let _serial = SERIAL.lock().await;
    use axum::response::IntoResponse;
    use futures_util::StreamExt as _;
    const COMPLETED: &str = r#"{"type":"response.completed","response":{"id":"r","status":"completed","output":[]}}"#;
    const CREATED: &str = r#"{"type":"response.created","response":{"id":"r","status":"in_progress","output":[]}}"#;
    let tmp = Path::new(env!("CARGO_TARGET_TMPDIR"));
    let Some(built) = cpa_plugin::testing::built_plugins(tmp, "plugin_host_model_bootstrap") else {
        return;
    };
    // 0: one data line, then EOF. 1: one whole event, then a reset. 2: one data line
    // without its blank line, then a reset (no deliverable payload).
    for case in 0..3 {
        let broken = case > 0;
        let mut unhooked = None;
        let upstream = {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let url = format!("http://{}", listener.local_addr().unwrap());
            let app = axum::Router::new().fallback(move || async move {
                let tail = broken.then(|| Err::<String, _>(std::io::Error::other("reset")));
                let event = match case {
                    0 => format!("data: {COMPLETED}"),
                    1 => format!("data: {CREATED}\n\n"),
                    _ => format!("data: {CREATED}\n"),
                };
                let body = futures_util::stream::iter(std::iter::once(Ok(event)).chain(tail)).then(|item| async move {
                    if item.is_err() {
                        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                    }
                    item
                });
                (
                    [("content-type", "text/event-stream")],
                    axum::body::Body::from_stream(body),
                )
                    .into_response()
            });
            tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
            url
        };
        for hooked in [false, true] {
            let dir = cpa_plugin::testing::scratch(tmp, &format!("bootstrap-{case}-{hooked}"));
            let (auths, records, plugins) = (dir.join("auths"), dir.join("records"), dir.join("plugins"));
            for d in [&auths, &records, &plugins] {
                std::fs::create_dir_all(d).unwrap();
            }
            let mut yaml = format!(
                "auth-dir: {}\napi-keys:\n  codex:\n    - base-url: {upstream}\n      models: [{{name: gpt-fixture}}]\n      keys: [{{api-key: sk-fake}}]\n",
                auths.display()
            );
            if hooked {
                std::fs::copy(built.join("recorder.so"), plugins.join("noop.so")).unwrap();
                yaml.push_str(&format!(
                    "plugins:\n  enabled: true\n  dir: {}\n  configs:\n    noop:\n      enabled: true\n      label: noop\n      record: {}\n      caps: request_interceptor,request_lifecycle_plugin\n",
                    plugins.display(),
                    records.display()
                ));
                for method in ["request.intercept_before", "request.intercept_after"] {
                    canned(&records, "noop", method, json!({}));
                }
            }
            let cfg = Config::parse(&yaml).unwrap();
            let credentials = cpa_core::config::credentials::load(&cfg);
            let rt = Arc::new(cpa_server::testing::runtime(
                cfg,
                credentials,
                Executors {
                    claude: cpa_exec::claude::ClaudeExecutor::new("http://127.0.0.1:1").unwrap(),
                    codex: cpa_exec::codex::CodexExecutor::new().unwrap(),
                    devices: Default::default(),
                    openai: Default::default(),
                    google: Default::default(),
                },
            ));
            cpa_server::plugins::start(&rt).await;
            assert_eq!(rt.plugins().has_request_interceptors(), hooked);
            let proxy = {
                let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
                let addr = listener.local_addr().unwrap();
                let app = cpa_server::router(rt.clone());
                tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
                addr
            };
            let response = wreq::Client::new()
                .post(format!("http://{proxy}/v1/responses"))
                .header("content-type", "application/json")
                .body(r#"{"model":"gpt-fixture","stream":true,"input":"hi"}"#)
                .send()
                .await
                .unwrap();
            let status = response.status().as_u16();
            let body = response.text().await.unwrap();
            let label = format!("case={case} hooked={hooked}");
            if case == 2 {
                // Hooks must not change the answer: the JSON error, as without hooks.
                assert_eq!(status, 500, "{label}: {body}");
                match &unhooked {
                    None => unhooked = Some(body),
                    Some(want) => assert_eq!(&body, want, "{label}"),
                }
                rt.plugins().shutdown_all(None).await;
                std::fs::remove_dir_all(dir).unwrap();
                continue;
            }
            let expected = if broken {
                // The pending event is delivered, then the error ends the stream.
                concat!(
                    r#"data: {"type":"response.created","response":{"id":"r","status":"in_progress","output":[],"model":"gpt-fixture"}}"#,
                    "\n\n\nevent: error\n",
                    r#"data: {"type":"error","error":{"code":"internal_server_error","message":"upstream request failed","param":null,"type":"server_error"},"sequence_number":1}"#,
                    "\n\n"
                )
                .to_owned()
            } else {
                format!("data: {COMPLETED}\n\n\n")
            };
            assert_eq!((status, body.as_str()), (200, expected.as_str()), "{label}");
            rt.plugins().shutdown_all(None).await;
            std::fs::remove_dir_all(dir).unwrap();
        }
    }
}
