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
        resolved_model: None,
        usage: Default::default(),
        request_path: String::new(),
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
        let translated = translate::request(&req, ctx.codex, &ctx.base_model, ctx.is_compat).unwrap();
        let prepared = if req.operation == Operation::CountTokens {
            ctx.prepare_count(&req, &translated).unwrap()
        } else {
            let original = translate::original(&req, &translated, ctx.codex, &ctx.base_model, ctx.is_compat).unwrap();
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
        resolved_model: None,
        usage: Default::default(),
        request_path: String::new(),
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

/// Go's conductor observes the quota headers of every Messages response (success,
/// undecodable success or upstream error) for Claude credentials; count_tokens results
/// skip observation.
#[tokio::test]
async fn messages_responses_record_the_quota_snapshot() {
    use axum::response::IntoResponse;
    let replies = Arc::new(std::sync::Mutex::new(std::collections::VecDeque::from([
        (200u16, "allowed"),
        (429, "rejected"),
        (200, "undecodable"),
        (200, "count-must-not-observe"),
    ])));
    let router = axum::Router::new().fallback(move || {
        let replies = replies.clone();
        async move {
            let (status, value) = replies.lock().unwrap().pop_front().unwrap();
            let body = if status == 200 {
                r#"{"id":"msg_q","type":"message","role":"assistant","model":"m","content":[],"stop_reason":"end_turn","usage":{"input_tokens":1,"output_tokens":1},"input_tokens":3}"#
            } else {
                r#"{"type":"error","error":{"type":"rate_limit_error","message":"limited"}}"#
            };
            // A 2xx whose gzip body cannot be decoded still observes the headers.
            let encoding = if value == "undecodable" { "gzip" } else { "identity" };
            (
                http::StatusCode::from_u16(status).unwrap(),
                [
                    ("content-type", "application/json"),
                    ("content-encoding", encoding),
                    ("anthropic-ratelimit-unified-status", value),
                    ("anthropic-workspace-id", "ws-not-a-signal"),
                ],
                body,
            )
                .into_response()
        }
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    let executor = ClaudeExecutor::with_client(wreq::Client::new(), DEFAULT_BASE_URL);
    let mut credential = Credential::from_file(
        Path::new("/fake"),
        Path::new("/fake/claude.json"),
        serde_json::json!({"type":"claude"}).as_object().unwrap().clone(),
    )
    .unwrap();
    credential
        .attributes
        .insert("api_key".into(), "fake-gateway-key".into());
    credential.attributes.insert("base_url".into(), base);
    let request = |operation| {
        let body = Bytes::from_static(br#"{"model":"m","max_tokens":8,"messages":[{"role":"user","content":"hi"}]}"#);
        ExecRequest {
            operation,
            source_format: Format::Claude,
            response_format: Format::Claude,
            requested_model: "m".into(),
            model: "m".into(),
            original_body: body.clone(),
            body,
            stream: false,
            alt: None,
            session: None,
            execution_session: None,
            derived_session: None,
            resolved_model: None,
            usage: Default::default(),
            request_path: String::new(),
            headers: Default::default(),
            caller: Caller {
                principal: "fake-client".into(),
                source: "x-api-key",
            },
        }
    };
    let cfg = Config::parse("").unwrap();
    let signal = |executor: &ClaudeExecutor| {
        let snapshot = executor.quota().snapshot(&credential.id).unwrap();
        assert_eq!(snapshot.signals.len(), 1, "only quota headers are signals");
        snapshot.signals["Anthropic-Ratelimit-Unified-Status"].clone()
    };
    assert!(executor.quota().snapshot(&credential.id).is_none());
    executor
        .execute(&credential, request(Operation::Generate), &cfg)
        .await
        .unwrap();
    assert_eq!(signal(&executor), "allowed");
    // Go also observes the result model's state (`model_quotas`).
    assert_eq!(
        executor.quota().model_snapshots(&credential.id).get("m"),
        executor.quota().snapshot(&credential.id).as_ref()
    );
    let error = executor
        .execute(&credential, request(Operation::Generate), &cfg)
        .await
        .err()
        .unwrap();
    assert_eq!(error.status, 429);
    assert_eq!(signal(&executor), "rejected");
    assert!(
        executor
            .execute(&credential, request(Operation::Generate), &cfg)
            .await
            .is_err()
    );
    assert_eq!(signal(&executor), "undecodable");
    // A custom origin counts locally; Delegation's upstream count still never observes.
    let count = Delegation {
        count_upstream: true,
        ..Default::default()
    };
    executor
        .execute_delegated(&credential, request(Operation::CountTokens), &cfg, count)
        .await
        .unwrap();
    assert_eq!(signal(&executor), "undecodable");
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

/// The model capabilities Go bound to the scenario's request (dispatch's job in Rust).
fn resolved(scenario: &Value) -> Option<cpa_core::exec::ResolvedModel> {
    let info = scenario.get("resolved_info")?.as_object()?.clone();
    Some(cpa_core::exec::ResolvedModel {
        info: cpa_core::registry::ModelInfo::from_raw(info).unwrap(),
        source: cpa_core::exec::ResolvedSource::ApiKey,
    })
}

/// [`normalize_random`] plus the values a cloaked request derives from the wall clock
/// and a fresh fake user ID.
fn normalize_replay(text: &str) -> String {
    let date = regex::Regex::new(r"Today's date is \d{4}-\d{2}-\d{2}").unwrap();
    let user = regex::Regex::new(r#"("user_id":")(?:[^"\\]|\\.)*""#).unwrap();
    let text = normalize_random(text);
    let text = date.replace_all(&text, "Today's date is <date>");
    user.replace_all(&text, "${1}<user>\"").into_owned()
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
    // Sequenced through execute(): compat_replay_sequence_matches_go; config keys:
    // m1_0041_to_m1_0056_config_keys_match_go.
    scenarios_match_go("all", |name| !name.starts_with("replay-") && !is_config_scenario(name)).await;
}

/// Scenarios whose only purpose is a `config.yaml` key (Go generator, same pipeline).
fn is_config_scenario(name: &str) -> bool {
    name.starts_with("config-") || name == "apikey-cloak-config-strict-sensitive"
}

/// M1-0041 rebuild-mid-system-message, M1-0043 cloak.strict-mode, M1-0044
/// cloak.sensitive-words, M1-0048..M1-0055 every header-defaults key (stabilized and
/// not) and M1-0056 disable-claude-cloak-mode, each set in config.yaml and run through
/// Go's executor by the scenario generator.
#[tokio::test]
async fn m1_0041_to_m1_0056_config_keys_match_go() {
    let fixture: Value = serde_json::from_str(include_str!("testdata/go_executor.json")).unwrap();
    let count = fixture["scenarios"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|s| is_config_scenario(s["name"].as_str().unwrap()))
        .count();
    assert_eq!(count, 5);
    scenarios_match_go("config", is_config_scenario).await;
}

async fn scenarios_match_go(run: &str, selected: fn(&str) -> bool) {
    let fixture: Value = serde_json::from_str(include_str!("testdata/go_executor.json")).unwrap();
    let root = std::env::temp_dir().join(format!("cpa-claude-go-{run}-{}", std::process::id()));
    let executor = ClaudeExecutor::with_client(wreq::Client::new(), DEFAULT_BASE_URL);
    let prompt_id = regex::Regex::new(r"cc_prompt_id=[0-9a-f-]{36};").unwrap();
    for scenario in fixture["scenarios"].as_array().unwrap() {
        let name = scenario["name"].as_str().unwrap();
        if !selected(name) {
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
            resolved_model: resolved(scenario),
            usage: Default::default(),
            request_path: String::new(),
            headers,
            caller: Caller {
                principal: scenario["client_key"].as_str().unwrap().into(),
                source: "authorization",
            },
        };
        let req = enrich(req);
        let mut ctx = Ctx::new(&executor, &credential, &req, &cfg, Default::default());
        ctx.today = scenario["date"].as_str().unwrap().into();
        let translated = translate::request(&req, ctx.codex, &ctx.base_model, ctx.is_compat).unwrap();
        let prepared = if count {
            ctx.prepare_count(&req, &translated)
        } else {
            // generate(): translated clients always stream upstream.
            let original = translate::original(&req, &translated, ctx.codex, &ctx.base_model, ctx.is_compat).unwrap();
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
            // The default User-Agent names the build: CLIProxyAPI/dev in Go's generator.
            if let Some(go_version) = b.strip_prefix("CLIProxyAPI/") {
                assert_eq!(go_version, "dev", "{name}: header {k}");
                assert_eq!(
                    a,
                    &format!("CLIProxyAPI/{}", env!("CARGO_PKG_VERSION")),
                    "{name}: header {k}"
                );
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
                Default::default(),
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

/// Go's non-stream native path: a reply whose OAuth tool alias cannot be restored
/// returns before ParseClaudeUsage, so the deferred TrackFailure publishes no tokens
/// even though the upstream body (and its model) was observed.
#[tokio::test]
async fn unrestorable_tool_alias_publishes_no_tokens() {
    use axum::response::IntoResponse;
    let router = axum::Router::new().fallback(|body: String| async move {
        // The client tool `other` went upstream as mcp__<virtual server>__<word>_other;
        // `query` under that server matches both passthrough MCP tools.
        let sent: Value = serde_json::from_str(&body).unwrap();
        let alias = sent["tools"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|t| t["name"].as_str())
            .find(|n| n.ends_with("_other"))
            .unwrap()
            .to_owned();
        let server = alias.split("__").nth(1).unwrap();
        let reply = serde_json::json!({
            "id": "msg_r", "type": "message", "role": "assistant", "model": "claude-upstream",
            "content": [{"type": "tool_use", "id": "toolu_1", "name": format!("mcp__{server}__query"), "input": {}}],
            "stop_reason": "tool_use", "usage": {"input_tokens": 11, "output_tokens": 3},
        });
        ([(http::header::CONTENT_TYPE, "application/json")], reply.to_string()).into_response()
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    let executor = ClaudeExecutor::with_client(wreq::Client::new(), DEFAULT_BASE_URL);
    let mut credential = harness_credential();
    credential.attributes.insert("base_url".into(), base);
    let body = Bytes::from_static(
        br#"{"model":"claude-sonnet-4-6","max_tokens":8,"messages":[{"role":"user","content":"hi"}],"tools":[{"name":"mcp__srv1__query","input_schema":{"type":"object"}},{"name":"mcp__srv2__query","input_schema":{"type":"object"}},{"name":"other","input_schema":{"type":"object"}}]}"#,
    );
    let usage = Arc::new(Usage::default());
    let req = enrich(ExecRequest {
        operation: Operation::Generate,
        source_format: Format::Claude,
        response_format: Format::Claude,
        requested_model: "claude-sonnet-4-6".into(),
        model: "claude-sonnet-4-6".into(),
        original_body: body.clone(),
        body,
        stream: false,
        alt: None,
        session: None,
        execution_session: None,
        derived_session: None,
        resolved_model: None,
        usage: cpa_core::exec::UsageSink::new(usage.clone()),
        request_path: String::new(),
        headers: Default::default(),
        caller: Caller {
            principal: "fake-client".into(),
            source: "authorization",
        },
    });
    let cfg = Config::parse("").unwrap();
    let error = executor.execute(&credential, req, &cfg).await.err().unwrap();
    let message = String::from_utf8_lossy(&error.body).into_owned();
    assert!(
        message.starts_with("restore Claude OAuth tool name from response: "),
        "{message}"
    );
    assert_eq!(
        usage.kinds(),
        ["request", "round_trip_started", "first_byte", "body", "publish_failure"]
    );
    let reports = usage.0.lock().unwrap();
    assert_eq!(
        reports.last().unwrap().2,
        format!("0 {message}"),
        "Go's plain error has no status"
    );
}

/// Go's reporter calls on each Claude path (claude_executor_execute.go and
/// claude_executor_stream.go): native Execute publishes, translated Execute and
/// ExecuteStream require usage (no EnsurePublished), the TTFT marks bracket the round
/// trip, a renamed upstream model is reported, and `responses/compact` returns before
/// any reporter exists.
#[tokio::test]
async fn usage_reports_follow_go_reporter_calls() {
    use axum::response::IntoResponse;
    let router = axum::Router::new().fallback(|body: String| async move {
        let model = gjson::get(&body, "model").str().to_owned();
        if gjson::get(&body, "stream").bool() {
            let events = format!(
                "event: message_start\ndata: {{\"type\":\"message_start\",\"message\":{{\"id\":\"msg_u\",\"type\":\"message\",\"role\":\"assistant\",\"model\":\"{model}\",\"content\":[],\"usage\":{{\"input_tokens\":3,\"output_tokens\":1}}}}}}\n\n\
                 event: message_delta\ndata: {{\"type\":\"message_delta\",\"delta\":{{\"stop_reason\":\"end_turn\"}},\"usage\":{{\"output_tokens\":2}}}}\n\n\
                 event: message_stop\ndata: {{\"type\":\"message_stop\"}}\n\n"
            );
            ([(http::header::CONTENT_TYPE, "text/event-stream")], events).into_response()
        } else {
            let reply = format!(
                r#"{{"id":"msg_u","type":"message","role":"assistant","model":"{model}","content":[{{"type":"text","text":"ok"}}],"stop_reason":"end_turn","usage":{{"input_tokens":3,"output_tokens":2}}}}"#
            );
            ([(http::header::CONTENT_TYPE, "application/json")], reply).into_response()
        }
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    let executor = ClaudeExecutor::with_client(wreq::Client::new(), DEFAULT_BASE_URL);
    let mut credential = Credential::from_file(
        Path::new("/fake"),
        Path::new("/fake/claude.json"),
        serde_json::json!({"type":"claude"}).as_object().unwrap().clone(),
    )
    .unwrap();
    credential
        .attributes
        .insert("api_key".into(), "fake-gateway-key".into());
    credential.attributes.insert("base_url".into(), base);
    let cfg = Config::parse("").unwrap();
    let run = |source: Format, stream: bool, alt: Option<&str>, delegation: Delegation| {
        let usage = Arc::new(Usage::default());
        let body = if source == Format::Claude {
            Bytes::from_static(
                br#"{"model":"claude-sonnet-4-6","max_tokens":8,"messages":[{"role":"user","content":"hi"}]}"#,
            )
        } else {
            Bytes::from_static(br#"{"model":"claude-sonnet-4-6","messages":[{"role":"user","content":"hi"}]}"#)
        };
        let req = ExecRequest {
            operation: Operation::Generate,
            source_format: source,
            response_format: source,
            requested_model: "claude-sonnet-4-6".into(),
            model: "claude-sonnet-4-6".into(),
            original_body: body.clone(),
            body,
            stream,
            alt: alt.map(str::to_owned),
            session: None,
            execution_session: None,
            derived_session: None,
            resolved_model: None,
            usage: cpa_core::exec::UsageSink::new(usage.clone()),
            request_path: String::new(),
            headers: Default::default(),
            caller: Caller {
                principal: "fake-client".into(),
                source: "x-api-key",
            },
        };
        let (executor, credential, cfg) = (&executor, &credential, &cfg);
        async move {
            let response = executor.execute_delegated(credential, req, cfg, delegation).await;
            if let Ok(ExecResponse {
                body: ResponseBody::Stream(events),
                ..
            }) = response
            {
                let _: Vec<_> = events.collect().await;
            }
            usage
        }
    };
    let without_lines = |usage: &Usage| usage.kinds().into_iter().filter(|k| *k != "line").collect::<Vec<_>>();
    // Native Execute: reporter.Publish(ParseClaudeUsage(data)).
    let usage = run(Format::Claude, false, None, Delegation::default()).await;
    assert_eq!(
        usage.kinds(),
        ["request", "round_trip_started", "first_byte", "body", "publish"]
    );
    // Translated Execute: streamUsage.Publish only, so usage is required.
    let usage = run(Format::OpenAI, false, None, Delegation::default()).await;
    assert_eq!(
        without_lines(&usage),
        ["request", "round_trip_started", "first_byte", "usage_required"]
    );
    assert!(usage.kinds().contains(&"line"));
    // ExecuteStream, native and translated: deferred streamUsage.Publish only.
    for source in [Format::Claude, Format::OpenAI] {
        let usage = run(source, true, None, Delegation::default()).await;
        assert_eq!(
            without_lines(&usage),
            ["request", "round_trip_started", "first_byte", "usage_required"],
            "{source:?}"
        );
    }
    // A delegation renaming the model: reporter.SetUpstreamModel.
    let renamed = Delegation {
        upstream_model: Some(|base| format!("{base}-upstream")),
        ..Default::default()
    };
    let usage = run(Format::Claude, false, None, renamed).await;
    assert_eq!(usage.kinds()[0], "upstream_model");
    assert_eq!(usage.0.lock().unwrap()[0].2, "claude-sonnet-4-6-upstream");
    // responses/compact: Go returns before NewExecutorUsageReporter.
    let usage = run(
        Format::OpenAIResponse,
        false,
        Some("responses/compact"),
        Delegation::default(),
    )
    .await;
    assert_eq!(usage.kinds(), ["discard"]);
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
    assert_eq!(steps.len(), 6);
    // Replies in the order requests reach upstream (steps Go failed locally send none).
    let replies: Vec<(u16, String)> = steps
        .iter()
        .filter(|s| s["upstream"].as_array().is_some_and(|u| !u.is_empty()))
        .map(|s| {
            (
                s["reply"]["status"].as_u64().unwrap() as u16,
                s["reply"]["body"].as_str().unwrap().to_owned(),
            )
        })
        .collect();
    let captured: Arc<std::sync::Mutex<Vec<String>>> = Arc::default();
    let seen = captured.clone();
    let router = axum::Router::new().fallback(move |body: String| {
        let seen = seen.clone();
        let replies = replies.clone();
        async move {
            let mut seen = seen.lock().unwrap();
            let (status, reply) = replies[seen.len()].clone();
            seen.push(body);
            (
                http::StatusCode::from_u16(status).unwrap(),
                [(http::header::CONTENT_TYPE, "application/json")],
                reply,
            )
                .into_response()
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
    let usage = Arc::new(Usage::default());
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
            resolved_model: resolved(scenario),
            usage: cpa_core::exec::UsageSink::new(usage.clone()),
            request_path: String::new(),
            headers,
            caller: Caller {
                principal: scenario["client_key"].as_str().unwrap().into(),
                source: "authorization",
            },
        });
        usage.0.lock().unwrap().clear();
        let sent = captured.lock().unwrap().len();
        let result = executor.execute(&credential, req, &cfg).await;
        match scenario["error_status"].as_u64() {
            Some(status) => match result {
                Err(error) => assert_eq!(u64::from(error.status), status, "step {step}"),
                Ok(_) => panic!("step {step}: Go failed with {status}"),
            },
            None => assert_eq!(result.map(|r| r.status).unwrap(), 200, "step {step}"),
        }
        let go = scenario["upstream"][0]["body"].as_str();
        let rust = captured.lock().unwrap().get(sent).cloned();
        assert_eq!(rust.is_some(), go.is_some(), "step {step}: upstream request");
        if let (Some(rust), Some(go)) = (rust.clone(), go) {
            assert_eq!(normalize_replay(&rust), normalize_replay(go), "step {step}");
        }
        // Go's usage reporter reads the body sent upstream (SetTranslatedReasoningEffort)
        // and the upstream reply as received (ParseClaudeUsage, ObserveResponseModel).
        let reports = usage.0.lock().unwrap().clone();
        let mut want = Vec::new();
        if let Some(sent) = rust {
            let reply = scenario["reply"]["body"].as_str().unwrap().to_owned();
            want.push(("request", Format::Claude, sent));
            want.push(("round_trip_started", Format::Claude, String::new()));
            if !reply.is_empty() {
                want.push(("first_byte", Format::Claude, String::new()));
            }
            // Native Execute publishes ParseClaudeUsage of the body; errors leave the
            // failure to the server (Go's deferred TrackFailure).
            if scenario["error_status"].is_null() {
                want.push(("body", Format::Claude, reply));
                want.push(("publish", Format::Claude, String::new()));
            }
        }
        assert_eq!(reports, want, "step {step}: usage reports");
    }
    let _ = std::fs::remove_dir_all(root);
}

/// Records what the executor reports to the usage queue.
#[derive(Default)]
pub(super) struct Usage(pub std::sync::Mutex<Vec<(&'static str, Format, String)>>);

impl cpa_core::exec::UsageObserver for Usage {
    fn response_body(&self, format: Format, body: &[u8]) {
        let body = String::from_utf8_lossy(body).into_owned();
        self.0.lock().unwrap().push(("body", format, body));
    }
    fn response_line(&self, format: Format, line: &[u8]) {
        let line = String::from_utf8_lossy(line).into_owned();
        self.0.lock().unwrap().push(("line", format, line));
    }
    fn request(&self, format: Format, payload: &[u8]) {
        let payload = String::from_utf8_lossy(payload).into_owned();
        self.0.lock().unwrap().push(("request", format, payload));
    }
    fn upstream_model(&self, model: &str) {
        self.push("upstream_model", model);
    }
    fn round_trip_started(&self) {
        self.push("round_trip_started", "");
    }
    fn first_byte(&self) {
        self.push("first_byte", "");
    }
    fn publish(&self) {
        self.push("publish", "");
    }
    fn publish_failure(&self, status: u16, body: &str) {
        self.push("publish_failure", &format!("{status} {body}"));
    }
    fn usage_required(&self) {
        self.push("usage_required", "");
    }
    fn discard(&self) {
        self.push("discard", "");
    }
}

impl Usage {
    fn push(&self, kind: &'static str, value: &str) {
        self.0.lock().unwrap().push((kind, Format::Claude, value.to_owned()));
    }

    fn kinds(&self) -> Vec<&'static str> {
        self.0.lock().unwrap().iter().map(|(kind, _, _)| *kind).collect()
    }
}
