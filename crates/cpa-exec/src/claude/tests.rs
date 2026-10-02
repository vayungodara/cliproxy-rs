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
        let req = request(case);
        let mut ctx = Ctx::new(&executor, &credential, &req, &cfg, Default::default());
        ctx.today = "2026-10-02".into();
        let translated = translate::request(&req).unwrap();
        let prepared = if req.operation == Operation::CountTokens {
            ctx.prepare_count(&req, &translated).unwrap()
        } else {
            ctx.prepare_messages(&req, &translated, req.stream).unwrap()
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

#[tokio::test]
async fn executor_scenarios_match_go() {
    let fixture: Value = serde_json::from_str(include_str!("testdata/go_executor.json")).unwrap();
    let root = std::env::temp_dir().join(format!("cpa-claude-go-{}", std::process::id()));
    let executor = ClaudeExecutor::with_client(wreq::Client::new(), DEFAULT_BASE_URL);
    for scenario in fixture["scenarios"].as_array().unwrap() {
        let name = scenario["name"].as_str().unwrap();
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
            source_format: Format::Claude,
            response_format: Format::Claude,
            requested_model: scenario["model"].as_str().unwrap().into(),
            model: scenario["model"].as_str().unwrap().into(),
            original_body: body.clone(),
            body,
            stream,
            alt: None,
            session: None,
            headers,
            caller: Caller {
                principal: scenario["client_key"].as_str().unwrap().into(),
                source: "authorization",
            },
        };
        let mut ctx = Ctx::new(&executor, &credential, &req, &cfg, Default::default());
        ctx.today = scenario["date"].as_str().unwrap().into();
        let translated = translate::request(&req).unwrap();
        let prepared = if count {
            ctx.prepare_count(&req, &translated).unwrap()
        } else {
            ctx.prepare_messages(&req, &translated, stream).unwrap()
        };
        let upstream = &scenario["upstream"][0];
        assert_eq!(
            normalize_random(&prepared.body),
            normalize_random(upstream["body"].as_str().unwrap()),
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
            if k == "X-Claude-Code-Session-Id" && name == "apikey-cloak-always" {
                continue;
            }
            assert_eq!(a, b, "{name}: header {k}");
        }
        let reply = upstream["reply"].as_str().unwrap();
        let output: Vec<String> = scenario["output"]
            .as_array()
            .map(|o| o.iter().map(|v| v.as_str().unwrap().to_owned()).collect())
            .unwrap_or_default();
        if scenario["error"].is_string() {
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
