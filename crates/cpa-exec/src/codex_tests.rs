//! Executor parity: each case in `tests/fixtures/codex_go.json` ran through Go's
//! `CodexExecutor` against a scripted loopback upstream. The Rust executor gets the same
//! request and upstream reply; the upstream request and the client-visible output must match.

use super::*;
use crate::codex_testkit::{Captured, GO, Mock, Reply};
use cpa_core::exec::Caller;
use serde_json::Value;
use std::path::Path;

fn credential(case: &Value, base_url: &str) -> Credential {
    let mut credential = Credential::from_file(
        Path::new("/fake"),
        Path::new("/fake/codex-fixture.json"),
        case["metadata"].as_object().cloned().unwrap_or_default(),
    )
    .unwrap_or_else(|| Credential {
        id: "codex-fixture.json".into(),
        provider: "codex".into(),
        source: cpa_core::credential::Source::File("/fake/codex-fixture.json".into()),
        disabled: false,
        label: "codex".into(),
        attributes: Default::default(),
        metadata: Default::default(),
        revision: 0,
    });
    credential.provider = "codex".into();
    credential.attributes.insert("base_url".into(), base_url.into());
    for (k, v) in case["attributes"].as_object().into_iter().flatten() {
        credential.attributes.insert(k.clone(), v.as_str().unwrap().into());
    }
    credential
}

fn request(case: &Value) -> ExecRequest {
    let source = Format::parse(case["source"].as_str().unwrap()).unwrap();
    let response = case["response"].as_str().and_then(Format::parse).unwrap_or(source);
    let mut headers = HeaderMap::new();
    for (k, v) in case["headers"].as_object().into_iter().flatten() {
        headers.insert(
            http::HeaderName::try_from(k.as_str()).unwrap(),
            v.as_str().unwrap().parse().unwrap(),
        );
    }
    let body = Bytes::from(case["payload"].as_str().unwrap().to_owned());
    ExecRequest {
        operation: Operation::Generate,
        source_format: source,
        response_format: response,
        requested_model: case["model"].as_str().unwrap().into(),
        model: case["model"].as_str().unwrap().into(),
        original_body: body.clone(),
        body,
        stream: case["stream"].as_bool().unwrap_or(false),
        alt: case["alt"].as_str().map(str::to_owned),
        session: None,
        headers,
        execution_session: None,
        derived_session: None,
        caller: Caller {
            principal: "client-key-FAKE".into(),
            source: "authorization",
        },
    }
}

fn reply(case: &Value) -> Reply {
    Reply {
        status: case["upstream_status"].as_u64().unwrap() as u16,
        headers: vec![
            ("content-type".into(), case["upstream_type"].as_str().unwrap().into()),
            ("x-codex-primary-used-percent".into(), "42".into()),
        ],
        body: case["upstream_body"].as_str().unwrap().into(),
    }
}

fn uuid_shaped(s: &str) -> bool {
    uuid::Uuid::parse_str(s).is_ok()
}

/// Same path, body bytes and header set. Generated session UUIDs only need the same shape.
fn assert_same_upstream(name: &str, rust: &Captured, go: &Value) {
    assert_eq!(rust.path, go["path"].as_str().unwrap(), "{name}: path");
    assert_eq!(
        String::from_utf8_lossy(&rust.body),
        go["body"].as_str().unwrap(),
        "{name}: upstream body"
    );
    let skip = |n: &str| matches!(n, "host" | "content-length");
    let go_headers = go["headers"].as_object().unwrap();
    for (name_, values) in go_headers {
        if skip(name_) {
            continue;
        }
        let expected = values[0].as_str().unwrap();
        let actual = rust.header(name_);
        if name_ == "session_id"
            && go_headers.contains_key("session_id")
            && !go["body"].as_str().unwrap().contains(expected)
        {
            assert!(
                uuid_shaped(actual),
                "{name}: {name_} should be a fresh UUID, got {actual:?}"
            );
            continue;
        }
        assert_eq!(actual, expected, "{name}: header {name_}");
    }
    for name_ in rust.headers.keys() {
        assert!(
            skip(name_.as_str()) || go_headers.contains_key(name_.as_str()),
            "{name}: extra header {name_}"
        );
    }
}

/// Go streams lines; Rust streams whole events. Compare event by event.
fn go_events(chunks: &Value) -> Vec<Vec<String>> {
    let mut events = Vec::new();
    let mut current = Vec::new();
    for chunk in chunks.as_array().unwrap() {
        match chunk.as_str().unwrap() {
            "" => events.push(std::mem::take(&mut current)),
            line => current.push(line.to_owned()),
        }
    }
    if !current.is_empty() {
        events.push(current);
    }
    events
}

fn rust_events(events: &[Bytes]) -> Vec<Vec<String>> {
    events
        .iter()
        .map(|e| {
            String::from_utf8_lossy(e)
                .trim_end_matches('\n')
                .split('\n')
                .map(str::to_owned)
                .collect()
        })
        .collect()
}

fn assert_same_error(name: &str, rust: &ExecError, go: &Value) {
    assert_eq!(rust.status, go["status"].as_u64().unwrap() as u16, "{name}: status");
    assert_eq!(
        String::from_utf8_lossy(&rust.body),
        go["message"].as_str().unwrap(),
        "{name}: message"
    );
    assert_eq!(
        rust.retry_after.map(|d| d.as_secs_f64()),
        go["retry_after_s"].as_f64(),
        "{name}: retry_after"
    );
    if go["credential_scoped"] == true {
        assert_eq!(rust.scope, FailureScope::Credential, "{name}: credential scoped");
    } else {
        assert_ne!(
            (rust.status, rust.scope),
            (429, FailureScope::Credential),
            "{name}: only usage limits cool the whole credential"
        );
    }
    if go["request_scoped"] == true {
        assert_eq!(rust.scope, FailureScope::Request, "{name}: request scoped");
    }
}

async fn run(
    executor: &CodexExecutor,
    case: &Value,
    mock: &Mock,
) -> (Result<ExecResponse, ExecError>, Vec<Result<Bytes, ExecError>>) {
    let credential = credential(case, &mock.url);
    let config_text = case["config"].as_str().filter(|s| !s.trim().is_empty()).unwrap_or("{}");
    let cfg = Config::parse(config_text).unwrap();
    let req = request(case);
    let ws_session = case["exec_metadata"]["execution_session_id"].as_str();
    let result = if ws_session.is_some() {
        let settings = Settings::from(&cfg);
        let view = View::new(&credential);
        executor.stream_with_session(&view, &settings, req, ws_session).await
    } else {
        executor.execute(&credential, req, &cfg).await
    };
    let mut items = Vec::new();
    let result = match result {
        Ok(ExecResponse {
            status,
            headers,
            body: ResponseBody::Stream(mut stream),
        }) => {
            while let Some(item) = stream.next().await {
                items.push(item);
            }
            Ok(ExecResponse {
                status,
                headers,
                body: ResponseBody::Buffered(Bytes::new()),
            })
        }
        other => other,
    };
    (result, items)
}

fn executor() -> CodexExecutor {
    CodexExecutor::with_client(wreq::Client::new(), CodexOAuth::new(wreq::Client::new()))
}

#[tokio::test]
async fn executor_matches_go_on_every_fixture_case() {
    let executor = executor();
    for case in GO["executor"].as_array().unwrap() {
        let name = case["name"].as_str().unwrap();
        let mock = Mock::start().await;
        let path = if case["alt"] == "responses/compact" {
            "/responses/compact"
        } else {
            "/responses"
        };
        mock.script(path, vec![reply(case)]);
        let (result, items) = run(&executor, case, &mock).await;
        let captured = mock.take();
        assert_eq!(captured.len(), 1, "{name}: one upstream request");
        assert_same_upstream(name, &captured[0], &case["upstream"]);
        let output = &case["output"];
        if let Some(go_error) = output.get("error") {
            assert_same_error(
                name,
                result
                    .as_ref()
                    .err()
                    .unwrap_or_else(|| panic!("{name}: expected error")),
                go_error,
            );
            continue;
        }
        let response = result.unwrap_or_else(|e| panic!("{name}: {e}"));
        if let Some(payload) = output["payload"].as_str() {
            let ResponseBody::Buffered(body) = response.body else {
                unreachable!()
            };
            assert_eq!(String::from_utf8_lossy(&body), payload, "{name}: payload");
            continue;
        }
        let ok: Vec<Bytes> = items.iter().filter_map(|i| i.as_ref().ok().cloned()).collect();
        assert_eq!(rust_events(&ok), go_events(&output["chunks"]), "{name}: events");
        match (items.last(), output.get("stream_error")) {
            (Some(Err(error)), Some(go)) => assert_same_error(name, error, go),
            (_, Some(_)) => panic!("{name}: expected a terminal stream error"),
            (Some(Err(error)), None) => panic!("{name}: unexpected stream error {error}"),
            _ => {}
        }
        let snapshot = executor.quota().snapshot("codex-fixture.json").expect("quota observed");
        assert_eq!(snapshot.signals["X-Codex-Primary-Used-Percent"], "42");
    }
}

#[tokio::test]
async fn prepare_refreshes_when_due_and_keeps_a_valid_token_on_failure() {
    use crate::codex_testkit::json;
    let mock = Mock::start().await;
    let oauth = CodexOAuth::with_endpoints(wreq::Client::new(), &format!("{}/oauth/token", mock.url), "", "")
        .with_retry_delay(std::time::Duration::from_millis(1));
    let executor = CodexExecutor::with_client(wreq::Client::new(), oauth);
    let cfg = Config::default();
    let soon = (Utc::now() + chrono::TimeDelta::hours(2)).to_rfc3339();
    let gone = (Utc::now() - chrono::TimeDelta::hours(2)).to_rfc3339();
    let cred = |refresh: &str, expired: &str| {
        Credential::from_file(
            Path::new("/fake"),
            Path::new("/fake/c.json"),
            serde_json::json!({"type":"codex","refresh_token":refresh,"access_token":"opaque","expired":expired})
                .as_object()
                .unwrap()
                .clone(),
        )
        .unwrap()
    };
    let due = cred("rt-1", &soon);
    assert!(executor.needs_prepare(&due, &cfg));
    mock.script("/oauth/token", vec![json(500, "{}"); 3]);
    let patch = executor.prepare(&due, &cfg).await.unwrap();
    assert!(patch.set.is_empty(), "valid token stays in use when refresh fails");
    assert_eq!(mock.take().len(), 3, "Go retries three times");

    let expired = cred("rt-2", &gone);
    mock.script("/oauth/token", vec![json(400, r#"{"error":"invalid_grant"}"#); 3]);
    let error = executor.prepare(&expired, &cfg).await.unwrap_err();
    assert_eq!((error.status, error.scope), (401, FailureScope::Credential));

    mock.script(
        "/oauth/token",
        vec![json(
            200,
            r#"{"access_token":"at-new","refresh_token":"rt-new","id_token":"","expires_in":3600}"#,
        )],
    );
    let patch = executor.prepare(&cred("rt-3", &gone), &cfg).await.unwrap();
    assert_eq!(patch.set["access_token"], "at-new");
    assert_eq!(patch.set["refresh_token"], "rt-new");
    assert!(!executor.needs_prepare(
        &cred("rt-4", &(Utc::now() + chrono::TimeDelta::days(3)).to_rfc3339()),
        &cfg
    ));
}

#[test]
fn alpha_search_body_rules_match_go_marshal() {
    for case in GO["alpha_search"].as_array().unwrap() {
        let input = case["input"].as_str().unwrap().as_bytes();
        let sanitized = sanitize_alpha_search(input);
        assert_eq!(
            String::from_utf8_lossy(&sanitized),
            case["sanitized"].as_str().unwrap(),
            "{case}"
        );
        let rewritten = rewrite_alpha_search_model(sanitized, "real-model");
        assert_eq!(
            String::from_utf8_lossy(&rewritten),
            case["rewritten"].as_str().unwrap(),
            "{case}"
        );
    }
}

#[test]
fn quota_events_and_signals_match_go() {
    for case in GO["quota"]["events"].as_array().unwrap() {
        let parsed = crate::codex_quota::event_headers(case["payload"].as_str().unwrap());
        let expected: Vec<(String, String)> = case["headers"]
            .as_object()
            .map(|m| {
                m.iter()
                    .map(|(k, v)| (k.to_ascii_lowercase(), v[0].as_str().unwrap().to_owned()))
                    .collect()
            })
            .unwrap_or_default();
        let mut actual: Vec<(String, String)> = parsed
            .unwrap_or_default()
            .into_iter()
            .map(|(k, v)| (k.to_ascii_lowercase(), v))
            .collect();
        actual.sort();
        let mut expected = expected;
        expected.sort();
        assert_eq!(actual, expected, "{}", case["payload"]);
    }
    let mut headers = HeaderMap::new();
    for (k, v) in [
        ("retry-after", "30"),
        ("x-codex-primary-used-percent", "42"),
        ("x-codex-plan-type", "pro"),
        ("x-ratelimit-limit-requests", "100"),
        ("x-codex-additional-foo-allowed", "true"),
        ("x-codex-unknown", "dropped"),
        ("anthropic-ratelimit-unified-status", "allowed"),
    ] {
        headers.insert(k, v.parse().unwrap());
    }
    headers.insert("x-codex-credits-balance", "9".repeat(600).parse().unwrap());
    headers.append("x-codex-secondary-used-percent", "1".parse().unwrap());
    headers.append("x-codex-secondary-used-percent", "2".parse().unwrap());
    let signals: serde_json::Map<String, Value> = crate::codex_quota::collect_signals(&headers)
        .into_iter()
        .map(|(k, v)| (k, Value::String(v)))
        .collect();
    assert_eq!(Value::Object(signals), GO["quota"]["signals"]);
}

fn oauth_credential(base_url: &str) -> Credential {
    let mut c = Credential::from_file(
        Path::new("/fake"),
        Path::new("/fake/codex-o.json"),
        serde_json::json!({"type":"codex","access_token":"at-FAKE","account_id":"acct"})
            .as_object()
            .unwrap()
            .clone(),
    )
    .unwrap();
    c.attributes.insert("base_url".into(), base_url.into());
    c
}

fn plain_request(stream: bool, alt: Option<&str>) -> ExecRequest {
    let body = Bytes::from_static(br#"{"model":"gpt-5.4","input":[]}"#);
    ExecRequest {
        operation: Operation::Generate,
        source_format: Format::Codex,
        response_format: Format::Codex,
        requested_model: "gpt-5.4".into(),
        model: "gpt-5.4".into(),
        original_body: body.clone(),
        body,
        stream,
        alt: alt.map(str::to_owned),
        session: None,
        headers: HeaderMap::new(),
        execution_session: None,
        derived_session: None,
        caller: Caller {
            principal: String::new(),
            source: "authorization",
        },
    }
}

#[tokio::test]
async fn compact_failures_follow_gos_availability_neutral_policy() {
    // Go: compact 404/405/501 and request faults stop the request without cooling;
    // other failures fail over without cooling; auth and quota keep the normal policy.
    use crate::codex_testkit::json;
    let mock = Mock::start().await;
    let executor = executor();
    let cfg = Config::default();
    for (status, body, scope) in [
        (404, "not here", FailureScope::Request),
        (405, "", FailureScope::Request),
        (501, "", FailureScope::Request),
        (500, "boom", FailureScope::Transport),
        (503, "busy", FailureScope::Transport),
        (401, "", FailureScope::Credential),
        (
            429,
            r#"{"error":{"type":"usage_limit_reached"}}"#,
            FailureScope::Credential,
        ),
    ] {
        mock.script("/responses/compact", vec![json(status, body)]);
        let error = executor
            .execute(
                &oauth_credential(&mock.url),
                plain_request(false, Some("responses/compact")),
                &cfg,
            )
            .await
            .err()
            .expect("compact error");
        assert_eq!((error.status, error.scope), (status, scope), "compact {status}");
    }
    // The same 404 on the ordinary Responses path is a model-support failure.
    mock.script("/responses", vec![json(404, "not here")]);
    let error = executor
        .execute(&oauth_credential(&mock.url), plain_request(false, None), &cfg)
        .await
        .err()
        .expect("responses error");
    assert_eq!(error.scope, FailureScope::Model);
}

#[tokio::test]
async fn truncated_error_body_is_a_transport_failure_not_an_auth_failure() {
    use axum::response::IntoResponse;
    let app = axum::Router::new().fallback(|| async {
        let broken = futures_util::stream::iter([
            Ok::<_, std::io::Error>(Bytes::from_static(b"{\"error\":")),
            Err(std::io::Error::other("mock reset mid-body")),
        ]);
        (
            axum::http::StatusCode::UNAUTHORIZED,
            [("content-type", "application/json")],
            axum::body::Body::from_stream(broken),
        )
            .into_response()
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await });
    let error = executor()
        .execute(&oauth_credential(&url), plain_request(true, None), &Config::default())
        .await
        .err()
        .expect("transport error");
    assert_eq!(error.scope, FailureScope::Transport, "{error}");
    assert_ne!(error.status, 401);
}

#[tokio::test]
async fn alpha_search_reads_at_most_32_mib_whatever_the_status() {
    use crate::codex_testkit::Reply;
    let mock = Mock::start().await;
    let big = "x".repeat(ALPHA_SEARCH_MAX_RESPONSE + 4096);
    let error_body = "e".repeat(100 * 1024);
    mock.script(
        "/alpha/search",
        vec![
            Reply {
                status: 200,
                headers: vec![("content-type".into(), "application/json".into())],
                body: big,
            },
            Reply {
                status: 500,
                headers: vec![("content-type".into(), "application/json".into())],
                body: error_body.clone(),
            },
        ],
    );
    let executor = executor().with_alpha_base_url(&mock.url);
    let cred = oauth_credential(&mock.url);
    for (status, expected) in [(200, ALPHA_SEARCH_MAX_RESPONSE), (500, error_body.len())] {
        let response = executor
            .alpha_search(&cred, b"{}", &HeaderMap::new(), "")
            .await
            .unwrap();
        assert_eq!(response.status, status);
        let ResponseBody::Buffered(body) = response.body else {
            unreachable!()
        };
        assert_eq!(body.len(), expected, "status {status}");
    }
}
