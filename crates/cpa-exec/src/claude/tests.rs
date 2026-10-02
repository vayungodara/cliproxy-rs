use super::*;
use cpa_core::exec::Caller;
use serde_json::Value;
use std::path::Path;
use std::time::Duration;

/// The harness credential (harness/fixtures.py): fake, local-only.
fn harness_credential() -> Credential {
    let metadata = serde_json::json!({
        "type": "claude", "access_token": "sk-ant-oat01-FAKE-LOCAL-ONLY",
        "refresh_token": "sk-ant-ort01-FAKE-LOCAL-ONLY", "id_token": "",
        "email": "local-fixture@example.invalid", "account_uuid": "aaaaaaaa-bbbb-4ccc-8ddd-eeeeeeeeeeee",
        "organization_uuid": "12345678-1234-4234-8234-123456789abc", "organization_name": "Local fixture",
        "claude_device_ids": ["0123456789abcdef".repeat(4)],
        "expired": "2099-01-01T00:00:00Z", "last_refresh": "2026-10-01T00:00:00Z",
    });
    Credential::from_file(
        Path::new("/auth"),
        Path::new("/auth/fixture.json"),
        metadata.as_object().unwrap().clone(),
    )
    .unwrap()
}

fn request(case: &Value) -> ExecRequest {
    let body = Bytes::from(case["request_body"].as_str().unwrap().to_owned());
    let headers: http::HeaderMap = case["request_headers"]
        .as_array()
        .unwrap()
        .iter()
        .map(|h| {
            (
                h[0].as_str().unwrap().parse().unwrap(),
                h[1].as_str().unwrap().parse().unwrap(),
            )
        })
        .collect();
    let count = case["path"].as_str().unwrap().ends_with("count_tokens");
    ExecRequest {
        operation: if count {
            Operation::CountTokens
        } else {
            Operation::Generate
        },
        source_format: Format::Claude,
        response_format: Format::Claude,
        requested_model: "claude-sonnet-4-6".into(),
        model: "claude-sonnet-4-6".into(),
        original_body: body.clone(),
        stream: rawjson::get(&String::from_utf8_lossy(&body), "stream").bool(),
        body,
        alt: None,
        session: None,
        execution_session: None,
        derived_session: None,
        headers,
        caller: Caller {
            principal: "fixture-client-key".into(),
            source: "authorization",
        },
    }
}

#[test]
fn pipeline_reproduces_go_upstream_captures() {
    let captures: Value = serde_json::from_str(include_str!("testdata/go_captures.json")).unwrap();
    let executor = ClaudeExecutor::with_client(wreq::Client::new(), DEFAULT_BASE_URL);
    let credential = harness_credential();
    let cfg = Config::parse("").unwrap();
    for case in captures["cases"].as_array().unwrap() {
        let name = case["name"].as_str().unwrap();
        let req = enrich(request(case));
        let mut ctx = Ctx::new(&executor, &credential, &req, &cfg, Default::default());
        ctx.today = "2026-10-02".into();
        let translated = translate::request(&req, &ctx.base_model, ctx.is_compat).unwrap();
        let prepared = if req.operation == Operation::CountTokens {
            ctx.prepare_count(&req, &translated).unwrap()
        } else {
            let original = translate::original(&req, &translated, &ctx.base_model, ctx.is_compat).unwrap();
            ctx.prepare_messages(&req, &translated, &original, req.stream).unwrap()
        };
        assert_eq!(prepared.body, case["upstream_body"].as_str().unwrap(), "{name}: body");
        let go: Vec<(String, String)> = case["upstream_headers"]
            .as_array()
            .unwrap()
            .iter()
            .map(|h| (h[0].as_str().unwrap().to_owned(), h[1].as_str().unwrap().to_owned()))
            .filter(|(k, _)| k != "Host" && k != "Content-Length")
            .collect();
        let names = |h: &[(String, String)]| h.iter().map(|(k, _)| k.clone()).collect::<Vec<_>>();
        assert_eq!(names(&prepared.headers), names(&go), "{name}: header order and casing");
        for ((k, ours), (_, theirs)) in prepared.headers.iter().zip(&go) {
            if k == "x-client-request-id" {
                assert!(uuid::Uuid::parse_str(ours).is_ok(), "{name}: request id");
                continue;
            }
            assert_eq!(ours, theirs, "{name}: {k}");
        }
        let order: Vec<_> = case["upstream_headers"]
            .as_array()
            .unwrap()
            .iter()
            .map(|h| h[0].as_str().unwrap())
            .collect();
        assert_eq!(
            prepared.order, order,
            "{name}: wire order including Host/Content-Length"
        );
    }
}

#[tokio::test]
async fn custom_origin_counts_locally_without_sending_credentials() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    // A first-party default must not override the credential's custom origin.
    let executor = ClaudeExecutor::with_client(wreq::Client::new(), DEFAULT_BASE_URL);
    let mut credential = Credential::from_file(
        Path::new("/fake"),
        Path::new("/fake/claude.json"),
        serde_json::json!({"type":"claude","access_token":"sk-ant-oat-fake"})
            .as_object()
            .unwrap()
            .clone(),
    )
    .unwrap();
    credential.attributes.insert("base_url".into(), base);
    let body = Bytes::from_static(br#"{"model":"claude","messages":[{"role":"user","content":"Hello."}]}"#);
    let req = ExecRequest {
        operation: Operation::CountTokens,
        source_format: Format::Claude,
        response_format: Format::Claude,
        requested_model: "claude".into(),
        model: "claude".into(),
        original_body: body.clone(),
        body,
        stream: false,
        alt: None,
        session: None,
        execution_session: None,
        derived_session: None,
        headers: Default::default(),
        caller: Caller {
            principal: "fake-client".into(),
            source: "x-api-key",
        },
    };
    let response = tokio::time::timeout(
        Duration::from_secs(2),
        executor.execute(&credential, req, &Config::parse("").unwrap()),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(response.status, 200);
    let ResponseBody::Buffered(body) = response.body else {
        panic!("count is buffered")
    };
    assert_eq!(body, br#"{"input_tokens":4}"#.as_slice());
    assert!(
        tokio::time::timeout(Duration::from_millis(20), listener.accept())
            .await
            .is_err()
    );
}

/// Go `session.Enrich` + `ExtractSessionID`, as the server applies them before the
/// executor (crates/cpa-server/src/session.rs, on the shared `cpa_common::session`).
fn enrich(mut req: ExecRequest) -> ExecRequest {
    use cpa_common::session::{self as shared, Meta};
    let scope = shared::caller_scope(&req.caller.principal);
    req.derived_session = shared::derived_id(
        req.source_format,
        &req.headers,
        &req.original_body,
        req.execution_session.as_deref(),
        &scope,
    );
    let meta = Meta {
        execution_session: req.execution_session.as_deref(),
        derived: req.derived_session.as_deref(),
    };
    let id = shared::extract_session_id(&req.headers, &req.original_body, &meta);
    req.session = (!id.is_empty()).then(|| shared::bound_session_identity(&id));
    req
}

/// Replaces values Go and Rust generate independently (random device/session IDs in
/// non-CLI fake user IDs, and the CCH computed over them) with fixed markers.
fn normalize_random(text: &str) -> String {
    let device = regex::Regex::new(r#"(device_id\\?":\\?")[0-9a-f]{64}"#).unwrap();
    let session = regex::Regex::new(r#"(session_id\\?":\\?")[0-9a-f-]{36}"#).unwrap();
    let cch = regex::Regex::new(r"cch=[0-9a-f]{5};").unwrap();
    let text = device.replace_all(text, "${1}<device>");
    let text = session.replace_all(&text, "${1}<session>");
    cch.replace_all(&text, "cch=<cch>;").into_owned()
}

/// Runs the scenario's upstream reply through the executor's response half (decoding,
/// error-body reading and classification) exactly as `send` does after the exchange.
async fn finish_reply(scenario: &Value, ctx: &Ctx<'_>, prepared: &Prepared) -> Result<RawResponse, ExecError> {
    use base64::Engine;
    let reply = &scenario["reply"];
    let mut headers = http::HeaderMap::new();
    headers.insert(
        http::header::CONTENT_TYPE,
        reply["content_type"].as_str().unwrap().parse().unwrap(),
    );
    for kv in reply["headers"].as_array().into_iter().flatten() {
        headers.append(
            http::HeaderName::from_bytes(kv[0].as_str().unwrap().as_bytes()).unwrap(),
            kv[1].as_str().unwrap().parse().unwrap(),
        );
    }
    let body = match reply["body_b64"].as_str() {
        Some(b64) => base64::engine::general_purpose::STANDARD.decode(b64).unwrap(),
        None => scenario["upstream"][0]["reply"].as_str().unwrap().as_bytes().to_vec(),
    };
    let upstream = crate::proxy::Upstream {
        status: reply["status"].as_u64().unwrap() as u16,
        headers,
        body: futures_util::stream::iter([Ok(Bytes::from(body))]).boxed(),
    };
    let fast = ctx.first_party && prepared.fast;
    finish(decode_upstream(upstream).await, fast, ctx.settings.model_level_cooling).await
}

/// The error Rust builds from the scenario's upstream reply matches what Go's conductor
/// and handlers read from its error: scope, retry hint, and the direct response.
fn assert_go_error(name: &str, scenario: &Value, info: &serde_json::Map<String, Value>, error: ExecError) {
    let status = scenario["error_status"]
        .as_u64()
        .or(info.get("direct_status").and_then(Value::as_u64))
        .map(|s| s as u16);
    let flag = |k: &str| info[k].as_bool().unwrap();
    let scope = if flag("request_scoped") {
        FailureScope::Request
    } else if flag("credential_scoped") {
        FailureScope::Credential
    } else if error.status == 429 {
        FailureScope::Model
    } else if status.is_none() {
        // A plain Go error: no status, scope or retry semantics.
        FailureScope::Transport
    } else {
        crate::upstream::scope_for(error.status)
    };
    assert_eq!(error.scope, scope, "{name}: scope");
    assert_eq!(error.retry_after.is_some(), flag("retry_after"), "{name}: retry hint");
    assert_eq!(error.direct, flag("direct"), "{name}: direct response");
    if let Some(status) = status {
        assert_eq!(error.status, status, "{name}: status");
    }
    if error.direct {
        assert_eq!(error.body, info["direct_body"].as_str().unwrap(), "{name}: direct body");
        let mut ours: Vec<(String, String)> = error
            .headers
            .iter()
            .map(|(k, v)| {
                (
                    crate::proxy::canonical_header(k.as_str()),
                    v.to_str().unwrap().to_owned(),
                )
            })
            .collect();
        ours.sort();
        let theirs: Vec<(String, String)> = info["direct_headers"]
            .as_array()
            .unwrap()
            .iter()
            .map(|kv| (kv[0].as_str().unwrap().to_owned(), kv[1].as_str().unwrap().to_owned()))
            .collect();
        assert_eq!(ours, theirs, "{name}: direct headers");
    } else {
        assert_eq!(error.body, scenario["error"].as_str().unwrap(), "{name}: error message");
    }
}

#[tokio::test]
async fn executor_scenarios_match_go() {
    let fixture: Value = serde_json::from_str(include_str!("testdata/go_executor.json")).unwrap();
    let root = std::env::temp_dir().join(format!("cpa-claude-go-{}", std::process::id()));
    let executor = ClaudeExecutor::with_client(wreq::Client::new(), DEFAULT_BASE_URL);
    let prompt_id = regex::Regex::new(r"cc_prompt_id=[0-9a-f-]{36};").unwrap();
    for scenario in fixture["scenarios"].as_array().unwrap() {
        let name = scenario["name"].as_str().unwrap();
        if name.starts_with("replay-") {
            // Sequenced through execute(): compat_replay_sequence_matches_go.
            continue;
        }
        let dir = root.join(name);
        std::fs::create_dir_all(dir.join("auth")).unwrap();
        let text = scenario["config"]
            .as_str()
            .unwrap()
            .replace("AUTH_DIR", dir.join("auth").to_str().unwrap());
        if let Some(auth) = scenario["auth_file"].as_str().filter(|s| !s.is_empty()) {
            std::fs::write(dir.join("auth").join("fixture.json"), auth).unwrap();
        }
        let cfg = Config::parse(&text).unwrap();
        let credential = cpa_core::config::credentials::load(&cfg)
            .into_iter()
            .find(|c| c.provider == "claude")
            .unwrap_or_else(|| panic!("{name}: no claude credential"));
        let body = Bytes::from(scenario["body"].as_str().unwrap().to_owned());
        let source = match scenario["source"].as_str() {
            Some("openai") => Format::OpenAI,
            Some("openai-response") => Format::OpenAIResponse,
            _ => Format::Claude,
        };
        let headers: http::HeaderMap = scenario["headers"]
            .as_array()
            .unwrap()
            .iter()
            .map(|h| {
                (
                    h[0].as_str().unwrap().parse().unwrap(),
                    h[1].as_str().unwrap().parse().unwrap(),
                )
            })
            .collect();
        let count = scenario["count"].as_bool().unwrap();
        let stream = scenario["stream"].as_bool().unwrap();
        let req = ExecRequest {
            operation: if count {
                Operation::CountTokens
            } else {
                Operation::Generate
            },
            source_format: source,
            response_format: source,
            requested_model: scenario["requested_model"]
                .as_str()
                .or(scenario["model"].as_str())
                .unwrap()
                .into(),
            model: scenario["model"].as_str().unwrap().into(),
            original_body: body.clone(),
            body,
            stream,
            alt: None,
            session: None,
            execution_session: scenario["execution_session"].as_str().map(str::to_owned),
            derived_session: None,
            headers,
            caller: Caller {
                principal: scenario["client_key"].as_str().unwrap().into(),
                source: "authorization",
            },
        };
        let req = enrich(req);
        let mut ctx = Ctx::new(&executor, &credential, &req, &cfg, Default::default());
        ctx.today = scenario["date"].as_str().unwrap().into();
        let translated = translate::request(&req, &ctx.base_model, ctx.is_compat).unwrap();
        let prepared = if count {
            ctx.prepare_count(&req, &translated)
        } else {
            // generate(): translated clients always stream upstream.
            let original = translate::original(&req, &translated, &ctx.base_model, ctx.is_compat).unwrap();
            ctx.prepare_messages(&req, &translated, &original, stream || source != Format::Claude)
        };
        if scenario["upstream"].as_array().is_none_or(Vec::is_empty) {
            // Go failed before sending (thinking validation and the like).
            let error = prepared
                .err()
                .unwrap_or_else(|| panic!("{name}: Go sent nothing, Rust prepared a request"));
            assert_eq!(error.body, scenario["error"].as_str().unwrap(), "{name}: error");
            assert_eq!(
                Some(u64::from(error.status)),
                scenario["error_status"].as_u64(),
                "{name}: status"
            );
            continue;
        }
        let prepared = prepared.unwrap_or_else(|e| panic!("{name}: {e}"));
        let upstream = &scenario["upstream"][0];
        // With execution metadata a new turn takes the continuity store's fresh random
        // prompt ID (uuid.NewString in Go) instead of the deterministic fingerprint one.
        let random_prompt = |text: &str| {
            if scenario["execution_session"].is_string() {
                prompt_id.replace_all(text, "cc_prompt_id=<random>;").into_owned()
            } else {
                text.to_owned()
            }
        };
        assert_eq!(
            random_prompt(&normalize_random(&prepared.body)),
            random_prompt(&normalize_random(upstream["body"].as_str().unwrap())),
            "{name}: upstream body"
        );
        let mut ours: Vec<(String, String)> = prepared.headers.clone();
        ours.sort_by(|a, b| a.0.cmp(&b.0));
        let theirs: Vec<(String, String)> = upstream["headers"]
            .as_array()
            .unwrap()
            .iter()
            .map(|h| (h[0].as_str().unwrap().to_owned(), h[1].as_str().unwrap().to_owned()))
            .collect();
        let names = |h: &[(String, String)]| h.iter().map(|(k, _)| k.clone()).collect::<Vec<_>>();
        assert_eq!(names(&ours), names(&theirs), "{name}: header names and casing");
        for ((k, a), (_, b)) in ours.iter().zip(&theirs) {
            if k.eq_ignore_ascii_case("x-client-request-id") && b != "9b6a3f8e-1c2d-4e5f-8a9b-0c1d2e3f4a5b" {
                continue;
            }
            if k == "X-Claude-Code-Session-Id" && name.starts_with("apikey-cloak") {
                continue;
            }
            assert_eq!(a, b, "{name}: header {k}");
        }
        let reply = upstream["reply"].as_str().unwrap();
        let output: Vec<String> = scenario["output"]
            .as_array()
            .map(|o| o.iter().map(|v| v.as_str().unwrap().to_owned()).collect())
            .unwrap_or_default();
        if source != Format::Claude {
            // Translated clients: only the upstream request is compared here.
            continue;
        }
        let finished = finish_reply(scenario, &ctx, &prepared).await;
        if let Some(info) = scenario["error_info"].as_object() {
            let error = finished
                .err()
                .unwrap_or_else(|| panic!("{name}: Go failed, Rust succeeded"));
            assert_go_error(name, scenario, info, error);
            continue;
        }
        if scenario["reply"]["body_b64"].is_string() {
            let Ok(RawResponse {
                body: ResponseBody::Stream(raw),
                ..
            }) = finished
            else {
                panic!("{name}: Rust failed where Go succeeded");
            };
            let decoded = collect(raw).await.unwrap();
            assert_eq!(decoded, output[0].as_bytes(), "{name}: decoded body");
            continue;
        }
        let request_id = scenario["reply"]["headers"]
            .as_array()
            .and_then(|h| h.iter().find(|kv| kv[0] == "Request-Id"))
            .map(|kv| kv[1].as_str().unwrap().to_owned())
            .unwrap_or_default();
        if count {
            assert_eq!(output, [reply], "{name}: count passthrough");
        } else if stream {
            let raw = futures_util::stream::iter([Ok(Bytes::from(reply.to_owned()))]).boxed();
            let continuity = prepared.continuity.clone();
            let events: Vec<String> = stream::relay(
                raw,
                prepared.reverse.clone(),
                Box::new(move |id| {
                    session::commit(
                        &continuity.key,
                        continuity.sequence,
                        &id,
                        &request_id,
                        &continuity.prompt_id,
                    )
                }),
            )
            .map(|e| String::from_utf8(e.unwrap().to_vec()).unwrap())
            .collect()
            .await;
            assert_eq!(events, output, "{name}: SSE events");
        } else {
            let restored = alias::restore_response(reply, &prepared.reverse).unwrap();
            assert_eq!([restored], output.as_slice(), "{name}: restored response");
            let c = &prepared.continuity;
            session::commit(
                &c.key,
                c.sequence,
                &rawjson::string(reply, "id"),
                &request_id,
                &c.prompt_id,
            );
        }
    }
    let _ = std::fs::remove_dir_all(root);
}

/// Go's in-process compat replay across two requests (store, then restore), replayed
/// through `execute()` against a local upstream that answers like Go's capture.
#[tokio::test]
async fn compat_replay_sequence_matches_go() {
    use axum::response::IntoResponse;
    let fixture: Value = serde_json::from_str(include_str!("testdata/go_executor.json")).unwrap();
    let steps: Vec<&Value> = fixture["scenarios"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|s| s["name"].as_str().unwrap().starts_with("replay-"))
        .collect();
    assert_eq!(steps.len(), 2);
    let replies: Vec<String> = steps
        .iter()
        .map(|s| s["reply"]["body"].as_str().unwrap().to_owned())
        .collect();
    let captured: Arc<std::sync::Mutex<Vec<String>>> = Arc::default();
    let seen = captured.clone();
    let router = axum::Router::new().fallback(move |body: String| {
        let seen = seen.clone();
        let replies = replies.clone();
        async move {
            let mut seen = seen.lock().unwrap();
            let reply = replies[seen.len()].clone();
            seen.push(body);
            ([(http::header::CONTENT_TYPE, "application/json")], reply).into_response()
        }
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    // The mock is an HTTP proxy, so the credential keeps Go's gateway base URL (the
    // config key match that binds is-compat uses it).
    let client = wreq::Client::builder()
        .proxy(wreq::Proxy::http(base.as_str()).unwrap())
        .build()
        .unwrap();
    let executor = ClaudeExecutor::with_client(client, DEFAULT_BASE_URL);
    let root = std::env::temp_dir().join(format!("cpa-claude-replay-{}", std::process::id()));
    std::fs::create_dir_all(&root).unwrap();
    for (step, scenario) in steps.iter().enumerate() {
        let text = scenario["config"]
            .as_str()
            .unwrap()
            .replace("AUTH_DIR", root.to_str().unwrap());
        let cfg = Config::parse(&text).unwrap();
        let credential = cpa_core::config::credentials::load(&cfg)
            .into_iter()
            .find(|c| c.provider == "claude")
            .unwrap();
        let body = Bytes::from(scenario["body"].as_str().unwrap().to_owned());
        let headers: http::HeaderMap = scenario["headers"]
            .as_array()
            .unwrap()
            .iter()
            .map(|h| {
                (
                    h[0].as_str().unwrap().parse().unwrap(),
                    h[1].as_str().unwrap().parse().unwrap(),
                )
            })
            .collect();
        let req = enrich(ExecRequest {
            operation: Operation::Generate,
            source_format: Format::Claude,
            response_format: Format::Claude,
            requested_model: scenario["requested_model"].as_str().unwrap().into(),
            model: scenario["model"].as_str().unwrap().into(),
            original_body: body.clone(),
            body,
            stream: false,
            alt: None,
            session: None,
            execution_session: None,
            derived_session: None,
            headers,
            caller: Caller {
                principal: scenario["client_key"].as_str().unwrap().into(),
                source: "authorization",
            },
        });
        let response = executor.execute(&credential, req, &cfg).await.unwrap();
        assert_eq!(response.status, 200);
        let rust = captured.lock().unwrap()[step].clone();
        let go = scenario["upstream"][0]["body"].as_str().unwrap();
        assert_eq!(normalize_random(&rust), normalize_random(go), "step {step}");
    }
    let _ = std::fs::remove_dir_all(root);
}
