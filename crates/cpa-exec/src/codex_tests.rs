//! Executor parity: each case in `tests/fixtures/codex_go.json` ran through Go's
//! `CodexExecutor` against a scripted loopback upstream. The Rust executor gets the same
//! request and upstream reply; the upstream request and the client-visible output must match.

use super::*;
use crate::codex_testkit::{Captured, GO, Mock, Reply, Tap, Wiretap};
use cpa_core::exec::Caller;
use cpa_core::gostr::GoStr;
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
    // Go's executor-side fallback (EnsureSessionContext -> CanonicalSessionID) for a
    // request without conductor metadata, which is how the fixtures were generated: the
    // canonical session from headers and body, with no derived identity.
    let session = Some(cpa_common::session::extract_session_id(
        &headers,
        &body,
        &Default::default(),
    ))
    .filter(|s| !s.is_empty())
    .map(|s| cpa_common::session::bound_session_identity(&s));
    ExecRequest {
        operation: Operation::Generate,
        source_format: source,
        response_format: response,
        stream_payloads: false,
        requested_model: case["model"].as_str().unwrap().into(),
        model: case["model"].as_str().unwrap().into(),
        original_body: body.clone(),
        body,
        stream: case["stream"].as_bool().unwrap_or(false),
        alt: case["alt"].as_str().map(str::to_owned),
        session,
        headers,
        execution_session: None,
        derived_session: None,
        resolved_model: None,
        usage: Default::default(),
        request_path: String::new(),
        caller: Caller {
            principal: "client-key-FAKE".into(),
            source: "authorization",
            provider: String::new(),
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
        if name_ == "referer" {
            // The previous hop's URL, on each run's own mock port.
            let path = |u: &str| url::Url::parse(u).map(|u| u.path().to_owned()).unwrap_or_default();
            assert_eq!(path(actual), path(expected), "{name}: header {name_}");
            continue;
        }
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

/// Go streams native lines one per chunk and translated events several per chunk; Rust
/// streams whole events. Compare event by event.
fn go_events(chunks: &Value) -> Vec<Vec<String>> {
    let mut events = Vec::new();
    let mut current = Vec::new();
    for chunk in chunks.as_array().unwrap() {
        let chunk = chunk.as_str().unwrap();
        let lines: Vec<&str> = if chunk.contains('\n') {
            chunk.split('\n').collect()
        } else {
            vec![chunk]
        };
        for line in lines {
            match line {
                "" => {
                    if !current.is_empty() {
                        events.push(std::mem::take(&mut current));
                    }
                }
                line => current.push(line.to_owned()),
            }
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
        .flat_map(|e| {
            String::from_utf8_lossy(e)
                .split("\n\n")
                .filter(|event| !event.trim_end_matches('\n').is_empty())
                .map(|event| event.trim_end_matches('\n').split('\n').map(str::to_owned).collect())
                .collect::<Vec<Vec<String>>>()
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
    let cfg = Config::parse(&config_text.replace("{{base_url}}", &mock.url)).unwrap();
    let mut req = request(case);
    if case["resolved_compat"] == true {
        // Go's conductor binds the configured model (`resolved_api_key_model_info`).
        let raw = serde_json::json!({"id": req.model, "is_compat": true});
        req.resolved_model = Some(cpa_core::exec::ResolvedModel {
            info: cpa_core::registry::ModelInfo::from_raw(raw.as_object().unwrap().clone()).unwrap(),
            source: cpa_core::exec::ResolvedSource::ApiKey,
        });
    }
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
        if let Some(redirect) = case["redirect"].as_u64() {
            let moved = format!("/moved{path}");
            mock.script(
                path,
                vec![Reply {
                    status: redirect as u16,
                    headers: vec![("location".into(), moved.clone())],
                    body: String::new(),
                }],
            );
            mock.script(&moved, vec![reply(case)]);
        } else {
            mock.script(path, vec![reply(case)]);
        }
        let (result, items) = run(&executor, case, &mock).await;
        let captured = mock.take();
        match case["hops"].as_array() {
            Some(hops) => {
                assert_eq!(captured.len(), hops.len(), "{name}: upstream requests");
                for (rust, go) in captured.iter().zip(hops) {
                    assert_eq!(rust.method, go["method"].as_str().unwrap(), "{name}: method");
                    assert_same_upstream(name, rust, go);
                }
            }
            None => {
                assert_eq!(captured.len(), 1, "{name}: one upstream request");
                assert_same_upstream(name, &captured[0], &case["upstream"]);
            }
        }
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
        // Go observes the same headers into the model's state (`model_quotas`).
        let model = cpa_common::thinking::parse_suffix(case["model"].as_str().unwrap()).model_name;
        let models = executor.quota().model_snapshots("codex-fixture.json");
        assert_eq!(models.get(&model), Some(&snapshot), "{name}: model quota");
    }
}

/// Go refreshes through `NewCodexAuthWithProxyURL(cfg, auth.ProxyURL)`: the credential's
/// proxy wins over the global one. A loopback HTTP proxy answers the token request, so
/// the unresolvable token host proves the request went through it.
#[tokio::test]
async fn refresh_uses_the_credential_proxy_before_the_global_one() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    async fn proxy_once(listener: tokio::net::TcpListener) -> String {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut buf = vec![0; 8192];
        let n = socket.read(&mut buf).await.unwrap();
        let body = r#"{"access_token":"at-proxied","refresh_token":"rt-proxied","id_token":"","expires_in":3600}"#;
        let reply = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        );
        socket.write_all(reply.as_bytes()).await.unwrap();
        String::from_utf8_lossy(&buf[..n]).lines().next().unwrap().to_owned()
    }
    let own = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let own_addr = own.local_addr().unwrap();
    let oauth = CodexOAuth::with_endpoints(wreq::Client::new(), "http://token.invalid/oauth/token", "", "");
    let executor = CodexExecutor::with_transport(crate::codex_tls::Transport::new(Default::default()), oauth);
    // The global proxy is a closed port: using it would fail the refresh.
    let cfg = Config::parse("proxy-url: http://127.0.0.1:9\n").unwrap();
    let gone = (Utc::now() - chrono::TimeDelta::hours(2)).to_rfc3339();
    let credential = Credential::from_file(
        Path::new("/fake"),
        Path::new("/fake/proxied.json"),
        serde_json::json!({
            "type": "codex",
            "refresh_token": "rt-proxy-test",
            "access_token": "opaque",
            "expired": gone,
            "proxy_url": format!("http://{own_addr}"),
        })
        .as_object()
        .unwrap()
        .clone(),
    )
    .unwrap();
    let seen = tokio::spawn(proxy_once(own));
    let patch = executor.prepare(&credential, &cfg).await.unwrap();
    assert_eq!(patch.set["access_token"], "at-proxied");
    assert_eq!(seen.await.unwrap(), "POST http://token.invalid/oauth/token HTTP/1.1");
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
        &cred("rt-4", &(Utc::now() + chrono::TimeDelta::days(5)).to_rfc3339()),
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
        stream_payloads: false,
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
        resolved_model: None,
        usage: Default::default(),
        request_path: String::new(),
        caller: Caller {
            principal: String::new(),
            source: "authorization",
            provider: String::new(),
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
            .alpha_search(
                &cred,
                b"{}",
                &HeaderMap::new(),
                "",
                &Config::default(),
                &Default::default(),
            )
            .await
            .unwrap();
        assert_eq!(response.status, status);
        let ResponseBody::Buffered(body) = response.body else {
            unreachable!()
        };
        assert_eq!(body.len(), expected, "status {status}");
    }
}

/// Records what the executor reports to the attempt's usage record.
#[derive(Default)]
struct Recorded(std::sync::Mutex<Vec<(&'static str, Format, String)>>);

impl cpa_core::exec::UsageObserver for Recorded {
    fn response_body(&self, format: Format, body: &[u8]) {
        self.0
            .lock()
            .unwrap()
            .push(("body", format, String::from_utf8_lossy(body).into_owned()));
    }
    fn response_line(&self, format: Format, line: &[u8]) {
        self.0
            .lock()
            .unwrap()
            .push(("line", format, String::from_utf8_lossy(line).into_owned()));
    }
    fn request(&self, format: Format, payload: &[u8]) {
        self.0
            .lock()
            .unwrap()
            .push(("request", format, String::from_utf8_lossy(payload).into_owned()));
    }
}

/// Usage records see the upstream side, as Go's reporter does: the finalized request
/// (`SetTranslatedReasoningEffort` on the upstream body, c7b4d573) and every
/// upstream event in Codex format, or the compaction body in OpenAI Responses format,
/// whatever the client speaks. Expected payloads come from the Go fixture.
#[tokio::test]
async fn usage_records_see_upstream_payloads() {
    let executor = executor();
    for name in [
        "apikey_claude_not_compat",
        "oauth_nonstream_no_instructions_free_plan",
        "oauth_compact",
    ] {
        let case = GO["executor"]
            .as_array()
            .unwrap()
            .iter()
            .find(|c| c["name"] == name)
            .unwrap();
        let mock = Mock::start().await;
        let compact = case["alt"] == "responses/compact";
        mock.script(
            if compact { "/responses/compact" } else { "/responses" },
            vec![reply(case)],
        );
        let credential = credential(case, &mock.url);
        let cfg = Config::parse(case["config"].as_str().filter(|s| !s.trim().is_empty()).unwrap_or("{}")).unwrap();
        let recorded = Arc::new(Recorded::default());
        let mut req = request(case);
        req.usage = cpa_core::exec::UsageSink::new(recorded.clone());
        match executor.execute(&credential, req, &cfg).await.unwrap().body {
            ResponseBody::Stream(mut stream) => while stream.next().await.is_some() {},
            ResponseBody::Buffered(_) => {}
        }
        let reports = recorded.0.lock().unwrap().clone();
        let upstream_format = if compact { Format::OpenAIResponse } else { Format::Codex };
        let sent = case["upstream"]["body"].as_str().unwrap().to_owned();
        assert_eq!(
            reports[0],
            ("request", upstream_format, sent),
            "{name}: translated request"
        );
        let upstream = case["upstream_body"].as_str().unwrap();
        let expected: Vec<(&str, Format, String)> = if compact {
            vec![("body", Format::OpenAIResponse, upstream.to_owned())]
        } else {
            upstream
                .lines()
                .filter_map(|l| l.strip_prefix("data:"))
                .map(|d| ("line", Format::Codex, d.trim().to_owned()))
                .collect()
        };
        assert_eq!(reports[1..], expected[..], "{name}: upstream payloads");
    }
}

/// Go `Test*RecordsFinalizedPayloadOverrideReasoningEffort` (c7b4d573): a payload
/// override of `reasoning.effort` reaches both the upstream body and the usage record,
/// on the stream, non-stream and compact paths. The record reads the finalized body, the
/// bytes sent upstream, so an input ID that sanitation prefixes is recorded prefixed.
#[tokio::test]
async fn usage_records_the_payload_override_reasoning_effort() {
    let executor = executor();
    for (name, protocol) in [
        ("apikey_claude_not_compat", "codex"),
        ("oauth_nonstream_no_instructions_free_plan", "codex"),
        ("oauth_compact", "openai-response"),
    ] {
        let case = GO["executor"]
            .as_array()
            .unwrap()
            .iter()
            .find(|c| c["name"] == name)
            .unwrap();
        let mock = Mock::start().await;
        let compact = case["alt"] == "responses/compact";
        mock.script(
            if compact { "/responses/compact" } else { "/responses" },
            vec![reply(case)],
        );
        let credential = credential(case, &mock.url);
        let cfg = Config::parse(&format!(
            "payload:\n  override:\n    - models:\n        - name: gpt-5.4\n          protocol: {protocol}\n      params:\n        reasoning.effort: high\n"
        ))
        .unwrap();
        let recorded = Arc::new(Recorded::default());
        let mut req = request(case);
        let body = String::from_utf8(req.body.to_vec()).unwrap().replace(
            r#"{"type":"message","role":"user""#,
            r#"{"type":"message","id":"abc","role":"user""#,
        );
        req.body = Bytes::from(body);
        req.usage = cpa_core::exec::UsageSink::new(recorded.clone());
        match executor.execute(&credential, req, &cfg).await.unwrap().body {
            ResponseBody::Stream(mut stream) => while stream.next().await.is_some() {},
            ResponseBody::Buffered(_) => {}
        }
        let sent = String::from_utf8(mock.take().pop().unwrap().body.to_vec()).unwrap();
        assert_eq!(gjson::get(&sent, "reasoning.effort").str(), "high", "{name}: upstream");
        let reports = recorded.0.lock().unwrap().clone();
        assert_eq!(reports[0].0, "request", "{name}");
        assert_eq!(reports[0].2, sent, "{name}: the usage record reads the sent body");
    }
}

/// Go `TestCodexImageResolvedModelOnWire` ("custom upstream", "routing and thinking
/// suffix" and the two "final payload" rows, 51bde0ff and 868e137) through the executor:
/// a configured, prefixed upstream name reaches the direct Images endpoint as resolved,
/// for a JSON generation and a multipart edit, and the payload rules run once, last.
#[tokio::test]
async fn direct_images_send_the_configured_upstream_model() {
    let executor = executor();
    let form = "--XB\r\nContent-Disposition: form-data; name=\"model\"\r\n\r\nREQ\r\n--XB\r\nContent-Disposition: form-data; name=\"prompt\"\r\n\r\ndiagnostic\r\n--XB--\r\n";
    let models = "[{name: openai/gpt-image-2-cpr, protocol: openai, match: [{model: openai/gpt-image-2-cpr}]}]";
    let override_rules = format!(
        "payload:\n  override:\n    - models: {models}\n      params: {{model: policy/image-model}}\n    - models: [{{name: \"*\"}}]\n      params: {{\"audit.-1\": once}}\n"
    );
    let filter_rules = format!("payload:\n  filter:\n    - models: {models}\n      params: [model]\n");
    for (requested, resolved, configured, want, rules) in [
        (
            "gpt-image-2",
            "openai/gpt-image-2-cpr",
            "openai/gpt-image-2-cpr",
            "openai/gpt-image-2-cpr",
            "",
        ),
        (
            "tenant/gpt-image-2(high)",
            "OpenAI/GPT-Image-2(high)",
            "OpenAI/GPT-Image-2",
            "OpenAI/GPT-Image-2",
            "",
        ),
        (
            "gpt-image-2",
            "openai/gpt-image-2-cpr",
            "openai/gpt-image-2-cpr",
            "policy/image-model",
            override_rules.as_str(),
        ),
        (
            "gpt-image-2",
            "openai/gpt-image-2-cpr",
            "openai/gpt-image-2-cpr",
            "",
            filter_rules.as_str(),
        ),
    ] {
        for (request_path, upstream_path, content_type, body) in [
            (
                "/v1/images/generations",
                "/images/generations",
                "application/json",
                format!(r#"{{"model":"{requested}","prompt":"diagnostic"}}"#),
            ),
            (
                "/v1/images/edits",
                "/images/edits",
                "multipart/form-data; boundary=XB",
                form.replace("REQ", requested),
            ),
        ] {
            let mock = Mock::start().await;
            mock.script(
                upstream_path,
                vec![crate::codex_testkit::json(
                    200,
                    r#"{"created":1713833628,"data":[{"b64_json":"AA=="}]}"#,
                )],
            );
            let cfg = Config::parse(&format!(
                "codex-api-key:\n  - api-key: codex-token\n    base-url: {}\n    prefix: tenant\n    models:\n      - name: {configured}\n        alias: gpt-image-2\n{rules}",
                mock.url
            ))
            .unwrap();
            let credential = cpa_core::config::credentials::from_config(&cfg)
                .into_iter()
                .find(|c| c.provider == "codex")
                .unwrap();
            let mut headers = HeaderMap::new();
            headers.insert(http::header::CONTENT_TYPE, content_type.parse().unwrap());
            let body = Bytes::from(body);
            let req = ExecRequest {
                operation: Operation::Generate,
                source_format: Format::OpenAI,
                response_format: Format::OpenAI,
                stream_payloads: false,
                requested_model: requested.into(),
                model: resolved.into(),
                original_body: body.clone(),
                body,
                stream: false,
                alt: None,
                session: None,
                headers,
                execution_session: None,
                derived_session: None,
                resolved_model: None,
                usage: Default::default(),
                request_path: request_path.into(),
                caller: Caller {
                    principal: "client-key-FAKE".into(),
                    source: "authorization",
                    provider: String::new(),
                },
            };
            executor.images(&credential, req, request_path, &cfg).await.unwrap();
            let captured = mock.take();
            assert_eq!(captured.len(), 1, "{resolved} {request_path}");
            assert_eq!(captured[0].path, upstream_path);
            let sent: Value = serde_json::from_slice(&captured[0].body).unwrap();
            assert_eq!(
                sent["model"].as_str().unwrap_or_default(),
                want,
                "{resolved} {request_path}"
            );
            assert_eq!(sent["prompt"], "diagnostic", "{request_path}");
            if rules.contains("audit") {
                assert_eq!(sent["audit"], serde_json::json!(["once"]), "{request_path}");
            }
        }
    }
}

/// The Images API cases (`GO["images"]`): Go's `CodexExecutor` ran each with source
/// format `openai-image`, the route in `request_path` metadata and the client headers in
/// the gin context, as the images handler calls it.
#[tokio::test]
async fn images_match_go_on_every_fixture_case() {
    use base64::Engine;
    let executor = executor();
    for case in GO["images"].as_array().unwrap() {
        let name = case["name"].as_str().unwrap();
        let mock = Mock::start().await;
        let path = case["upstream"]["path"].as_str().unwrap_or("/responses");
        mock.script(path, vec![reply(case)]);
        let credential = credential(case, &mock.url);
        let config_text = case["config"].as_str().filter(|s| !s.trim().is_empty()).unwrap_or("{}");
        let cfg = Config::parse(config_text).unwrap();
        let mut headers = HeaderMap::new();
        for (k, v) in case["headers"].as_object().into_iter().flatten() {
            headers.insert(
                http::HeaderName::try_from(k.as_str()).unwrap(),
                v.as_str().unwrap().parse().unwrap(),
            );
        }
        let body = Bytes::from(
            base64::engine::general_purpose::STANDARD
                .decode(case["payload_b64"].as_str().unwrap())
                .unwrap(),
        );
        let request_path = case["request_path"].as_str().unwrap();
        let model = case["model"].as_str().unwrap();
        let req = ExecRequest {
            operation: Operation::Generate,
            source_format: Format::OpenAI,
            response_format: Format::OpenAI,
            stream_payloads: false,
            requested_model: model.into(),
            model: model.into(),
            original_body: body.clone(),
            body,
            stream: case["stream"].as_bool().unwrap_or(false),
            alt: None,
            session: None,
            headers,
            execution_session: None,
            derived_session: None,
            resolved_model: None,
            usage: Default::default(),
            request_path: request_path.into(),
            caller: Caller {
                principal: "client-key-FAKE".into(),
                source: "authorization",
                provider: String::new(),
            },
        };
        let result = executor.images(&credential, req, request_path, &cfg).await;
        let captured = mock.take();
        match case.get("upstream").filter(|u| !u.is_null()) {
            None => assert!(captured.is_empty(), "{name}: no upstream request"),
            Some(go) => {
                assert_eq!(captured.len(), 1, "{name}: one upstream request");
                let mut go = go.clone();
                if name == "direct_edit_multipart" {
                    // Go ranges over its form map: field order varies per run.
                    let go_body: Value = serde_json::from_str(go["body"].as_str().unwrap()).unwrap();
                    let rust_body: Value = serde_json::from_slice(&captured[0].body).unwrap();
                    assert_eq!(rust_body, go_body, "{name}: upstream body fields");
                    go["body"] = Value::String(String::from_utf8_lossy(&captured[0].body).into_owned());
                }
                assert_same_upstream(name, &captured[0], &go);
            }
        }
        let output = &case["output"];
        if let Some(go_error) = output.get("error") {
            let error = result.err().unwrap_or_else(|| panic!("{name}: expected error"));
            match go_error.get("status") {
                Some(_) => assert_same_error(name, &error, go_error),
                // A plain Go error: the handler answers 500.
                None => {
                    assert_eq!(error.status, 500, "{name}: status");
                    assert_eq!(
                        String::from_utf8_lossy(&error.body),
                        go_error["message"].as_str().unwrap(),
                        "{name}: message"
                    );
                }
            }
            continue;
        }
        let response = result.unwrap_or_else(|e| panic!("{name}: {e}"));
        match response.body {
            ResponseBody::Buffered(body) => {
                assert_eq!(
                    String::from_utf8_lossy(&body),
                    output["payload"]
                        .as_str()
                        .unwrap_or_else(|| panic!("{name}: Go streamed")),
                    "{name}: payload"
                );
            }
            ResponseBody::Stream(mut stream) => {
                let mut items = Vec::new();
                while let Some(item) = stream.next().await {
                    items.push(item);
                }
                let joined: String = items
                    .iter()
                    .filter_map(|i| i.as_ref().ok())
                    .map(|b| String::from_utf8_lossy(b).into_owned())
                    .collect();
                let go_joined: String = output["chunks"]
                    .as_array()
                    .unwrap_or_else(|| panic!("{name}: Go answered without a stream"))
                    .iter()
                    .map(|c| c.as_str().unwrap())
                    .collect();
                assert_eq!(joined, go_joined, "{name}: stream bytes");
                match (items.last(), output.get("stream_error")) {
                    (Some(Err(error)), Some(go)) => assert_same_error(name, error, go),
                    (_, Some(_)) => panic!("{name}: expected a terminal stream error"),
                    (Some(Err(error)), None) => panic!("{name}: unexpected stream error {error}"),
                    _ => {}
                }
            }
        }
    }
}

/// Go `Auth.AccountInfo` for the fixture credentials.
fn go_account(credential: &Credential) -> [String; 5] {
    let (kind, value) = match credential.attributes.get("api_key") {
        Some(key) => ("api_key", key.clone()),
        None => ("oauth", credential.str("email").unwrap_or_default().to_owned()),
    };
    [
        "codex".into(),
        credential.id.clone(),
        credential.label.clone(),
        kind.into(),
        value,
    ]
}

/// Upstream capture (M4-0250) at Go's Codex HTTP sites (`codex_executor_execute.go:93,264`,
/// `codex_executor_stream.go:101`). Go records the request header map and body as sent,
/// the response status, the error body or, on success, the whole body (Execute, compact)
/// or each scanned line (streams), and every stream failure it reports. So for each case
/// of Go's executor fixture: the recorded request is what the upstream received (less
/// the transport's own headers), the chunks are the upstream body, and the recorded
/// stream error is Go's.
#[tokio::test]
async fn capture_records_each_attempt_as_go_does() {
    let executor = executor();
    for case in GO["executor"].as_array().unwrap() {
        let name = case["name"].as_str().unwrap();
        let mock = Mock::start().await;
        let path = if case["alt"] == "responses/compact" {
            "/responses/compact"
        } else {
            "/responses"
        };
        if let Some(redirect) = case["redirect"].as_u64() {
            let moved = format!("/moved{path}");
            mock.script(
                path,
                vec![Reply {
                    status: redirect as u16,
                    headers: vec![("location".into(), moved.clone())],
                    body: String::new(),
                }],
            );
            mock.script(&moved, vec![reply(case)]);
        } else {
            mock.script(path, vec![reply(case)]);
        }
        let credential = credential(case, &mock.url);
        let cfg = Config::parse(case["config"].as_str().filter(|s| !s.trim().is_empty()).unwrap_or("{}")).unwrap();
        let tap = Arc::new(Wiretap::default());
        let mut req = request(case);
        req.usage = cpa_core::exec::UsageSink::default().with_capture(cpa_core::exec::CaptureSink::new(tap.clone()));
        let ws_session = case["exec_metadata"]["execution_session_id"].as_str();
        let result = if ws_session.is_some() {
            let settings = Settings::from(&cfg);
            executor
                .stream_with_session(&View::new(&credential), &settings, req, ws_session)
                .await
        } else {
            executor.execute(&credential, req, &cfg).await
        };
        if let Ok(ExecResponse {
            body: ResponseBody::Stream(mut stream),
            ..
        }) = result
        {
            while stream.next().await.is_some() {}
        }
        let taps = tap.taps();
        let sent = &mock.take()[0];
        // The request: Go's header map is what went out, less what the transport adds.
        let Tap::Request {
            url,
            method,
            headers,
            body,
            account,
        } = &taps[0]
        else {
            panic!("{name}: first event {:?}", taps[0]);
        };
        assert_eq!(
            (url.as_str(), method.as_str()),
            (format!("{}{path}", mock.url).as_str(), "POST"),
            "{name}"
        );
        assert_eq!(body.as_bytes(), sent.body.as_ref(), "{name}: request body");
        assert_eq!(*account, go_account(&credential), "{name}: account");
        let mut recorded: Vec<(String, String)> = headers.iter().map(|(k, v)| (k.go_lower(), v.clone())).collect();
        recorded.sort();
        let mut wire: Vec<(String, String)> = sent
            .headers
            .iter()
            .filter(|(k, _)| !matches!(k.as_str(), "host" | "content-length" | "accept-encoding"))
            .map(|(k, v)| (k.to_string(), v.to_str().unwrap().to_owned()))
            .collect();
        wire.sort();
        assert_eq!(recorded, wire, "{name}: request headers");
        assert!(
            headers.iter().all(|(k, _)| *k == crate::proxy::canonical_header(k)),
            "{name}: Go header spelling {headers:?}"
        );
        let status = case["upstream_status"].as_u64().unwrap() as u16;
        assert!(
            matches!(&taps[1], Tap::Metadata(s, _) if *s == status),
            "{name}: {:?}",
            taps[1]
        );
        let upstream = case["upstream_body"].as_str().unwrap();
        let rest = &taps[2..];
        let chunks: Vec<&str> = rest
            .iter()
            .filter_map(|t| match t {
                Tap::Chunk(c) if !c.trim().is_empty() => Some(c.as_str()),
                _ => None,
            })
            .collect();
        let errors: Vec<&str> = rest
            .iter()
            .filter_map(|t| match t {
                Tap::Error(e) => Some(e.as_str()),
                _ => None,
            })
            .collect();
        if !(200..300).contains(&status) || case["stream"] == false {
            // The error body, or Execute's and compact's whole body, recorded once.
            assert_eq!(chunks, [upstream], "{name}: whole body");
            assert!(
                errors.is_empty(),
                "{name}: Execute records no parse failure: {errors:?}"
            );
            continue;
        }
        // Streams record every scanned line up to where Go stops reading.
        let lines: Vec<&str> = upstream.split('\n').filter(|l| !l.trim().is_empty()).collect();
        assert!(!chunks.is_empty(), "{name}: stream lines");
        assert_eq!(chunks, lines[..chunks.len()], "{name}: stream lines");
        match case["output"].get("stream_error") {
            Some(go) => assert_eq!(errors, [go["message"].as_str().unwrap()], "{name}: stream error"),
            None if case["output"].get("error").is_some() => {
                assert_eq!(errors.len(), 1, "{name}: the bootstrap failure: {errors:?}")
            }
            None => {
                assert_eq!(chunks.len(), lines.len(), "{name}: every line");
                assert!(errors.is_empty(), "{name}: {errors:?}");
            }
        }
    }
}

/// Go's transport failure path: the request, then `RecordAPIResponseError`.
#[tokio::test]
async fn capture_records_a_transport_failure() {
    let tap = Arc::new(Wiretap::default());
    let mut req = plain_request(true, None);
    req.usage = cpa_core::exec::UsageSink::default().with_capture(cpa_core::exec::CaptureSink::new(tap.clone()));
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    drop(listener);
    let error = executor()
        .execute(&oauth_credential(&url), req, &Config::default())
        .await
        .err()
        .expect("nothing listens");
    let taps = tap.taps();
    assert!(matches!(&taps[0], Tap::Request { .. }), "{taps:?}");
    assert_eq!(
        taps[1..],
        [Tap::Error(String::from_utf8_lossy(&error.body).into_owned())]
    );
}

/// A loopback upstream answering every request with `status`, `content_type` and these
/// body pieces, the last one an error when `reset` (the connection breaks mid-body).
async fn broken_upstream(status: u16, content_type: &'static str, pieces: Vec<&'static str>, reset: bool) -> String {
    use axum::response::IntoResponse;
    let app = axum::Router::new().fallback(move || {
        let pieces = pieces.clone();
        async move {
            let items: Vec<Result<Bytes, std::io::Error>> = pieces
                .into_iter()
                .map(|p| Ok(Bytes::from_static(p.as_bytes())))
                .collect();
            // The reset comes after the bytes were flushed, as a peer dying mid-body.
            let reset = futures_util::stream::iter(reset.then_some(())).then(|()| async {
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                Err::<Bytes, _>(std::io::Error::other("mock reset mid-body"))
            });
            (
                axum::http::StatusCode::from_u16(status).unwrap(),
                [("content-type", content_type)],
                axum::body::Body::from_stream(futures_util::stream::iter(items).chain(reset)),
            )
                .into_response()
        }
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await });
    url
}

fn tapped_request(stream: bool, alt: Option<&str>) -> (ExecRequest, Arc<Wiretap>) {
    let tap = Arc::new(Wiretap::default());
    let mut req = plain_request(stream, alt);
    req.usage = cpa_core::exec::UsageSink::default().with_capture(cpa_core::exec::CaptureSink::new(tap.clone()));
    (req, tap)
}

/// Go's stream loop records each line as it scans it and returns at the terminal event,
/// so a line after `response.completed` in the same SSE event is never recorded.
#[tokio::test]
async fn capture_stops_at_the_terminal_line() {
    const COMPLETED: &str = r#"data: {"type":"response.completed","response":{"id":"r","output":[]}}"#;
    let url = broken_upstream(
        200,
        "text/event-stream",
        vec![
            "data: {\"type\":\"response.created\",\"response\":{\"id\":\"r\"}}\n\n",
            "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"r\",\"output\":[]}}\nid: after-terminal\n\n",
        ],
        false,
    )
    .await;
    let (req, tap) = tapped_request(true, None);
    let response = executor()
        .execute(&oauth_credential(&url), req, &Config::default())
        .await
        .unwrap();
    let ResponseBody::Stream(mut stream) = response.body else {
        panic!("a stream");
    };
    while stream.next().await.is_some() {}
    let chunks: Vec<String> = tap
        .taps()
        .into_iter()
        .filter_map(|t| match t {
            Tap::Chunk(c) if !c.is_empty() => Some(c),
            _ => None,
        })
        .collect();
    assert_eq!(chunks.last().map(String::as_str), Some(COMPLETED), "{chunks:?}");
    assert!(!chunks.iter().any(|c| c.contains("after-terminal")), "{chunks:?}");
}

/// Body read failures follow each Go call site: Execute (`data, errRead := io.ReadAll`)
/// records every byte that arrived, the unfinished tail included, then the error; Alpha
/// Search records the partial body, then the error; the stream path's error response
/// records only the read error and returns it.
#[tokio::test]
async fn capture_keeps_each_sites_read_error_policy() {
    // Execute: the whole partial body, then the error.
    let url = broken_upstream(
        200,
        "text/event-stream",
        vec!["data: {\"type\":\"response.created\",\"response\":{\"id\":\"r\"}}\n\ndata: {\"type\":\"resp"],
        true,
    )
    .await;
    let (req, tap) = tapped_request(false, None);
    let result = executor()
        .execute(&oauth_credential(&url), req, &Config::default())
        .await;
    let taps = tap.taps();
    assert!(taps.len() > 2, "{taps:?} {:?}", result.err());
    assert_eq!(
        taps[2],
        Tap::Chunk(
            "data: {\"type\":\"response.created\",\"response\":{\"id\":\"r\"}}\n\ndata: {\"type\":\"resp".into()
        ),
        "{taps:?}"
    );
    assert!(matches!(&taps[3], Tap::Error(_)), "{taps:?}");
    assert_eq!(taps.len(), 4, "{taps:?}");

    // Alpha Search: the prefix, then the error.
    let url = broken_upstream(200, "application/json", vec!["{\"results\":["], true).await;
    let tap = Arc::new(Wiretap::default());
    let mut cred = oauth_credential(&url);
    cred.attributes.insert("api_key".into(), "sk-FAKE".into());
    let _ = executor()
        .alpha_search(
            &cred,
            b"{}",
            &HeaderMap::new(),
            "",
            &Config::default(),
            &cpa_core::exec::CaptureSink::new(tap.clone()),
        )
        .await;
    let taps = tap.taps();
    assert_eq!(taps[2], Tap::Chunk("{\"results\":[".into()), "{taps:?}");
    assert!(matches!(&taps[3], Tap::Error(_)), "{taps:?}");

    // Stream: an error body that breaks is the read error, not a status error.
    let url = broken_upstream(401, "application/json", vec!["{\"error\":"], true).await;
    let (req, tap) = tapped_request(true, None);
    let error = executor()
        .execute(&oauth_credential(&url), req, &Config::default())
        .await
        .err()
        .expect("read error");
    assert_ne!(error.status, 401, "{error}");
    let taps = tap.taps();
    assert_eq!(
        taps[2..],
        [Tap::Error(String::from_utf8_lossy(&error.body).into_owned())],
        "{taps:?}"
    );
}

/// The direct Images endpoints (`executeDirectOpenAIImage` and its stream form) read a
/// non-2xx body with `io.ReadAll` and, when it breaks, record the read error and return
/// it in place of the status error.
#[tokio::test]
async fn direct_images_return_the_read_error_of_a_broken_error_body() {
    for stream in [false, true] {
        let url = broken_upstream(401, "application/json", vec!["{\"error\":"], true).await;
        let (mut req, tap) = tapped_request(stream, None);
        let body = Bytes::from_static(br#"{"model":"gpt-image-2","prompt":"a lighthouse"}"#);
        req.source_format = Format::OpenAI;
        req.response_format = Format::OpenAI;
        req.requested_model = "gpt-image-2".into();
        req.model = "gpt-image-2".into();
        req.original_body = body.clone();
        req.body = body;
        let error = executor()
            .images(
                &oauth_credential(&url),
                req,
                "/v1/images/generations",
                &Config::default(),
            )
            .await
            .err()
            .expect("read error");
        assert_ne!(error.status, 401, "stream={stream}: {error}");
        let taps = tap.taps();
        assert!(matches!(&taps[0], Tap::Request { .. }), "stream={stream}: {taps:?}");
        assert_eq!(
            taps[2..],
            [Tap::Error(String::from_utf8_lossy(&error.body).into_owned())],
            "stream={stream}: {taps:?}"
        );
    }
}

/// A streamed (or buffered, `stream: false`) Responses request whose upstream sends `sse`:
/// the client frames and the in-stream error, or the attempt's error.
async fn through_sse(sse: &str, stream: bool, buffering: bool) -> Result<(Vec<String>, Option<ExecError>), ExecError> {
    let mock = Mock::start().await;
    mock.script(
        "/responses",
        vec![Reply {
            status: 200,
            headers: vec![("content-type".into(), "text/event-stream".into())],
            body: sse.into(),
        }],
    );
    let synthetic = serde_json::json!({
        "source": "openai-response",
        "model": "gpt-5.4",
        "payload": r#"{"model":"gpt-5.4","input":"hello"}"#,
        "stream": stream,
        "attributes": {"api_key": "test"},
    });
    let cfg = Config::parse(&format!("codex:\n  stream-bootstrap-buffering: {buffering}\n")).unwrap();
    let response = executor()
        .execute(&credential(&synthetic, &mock.url), request(&synthetic), &cfg)
        .await?;
    let (mut frames, mut failure) = (Vec::new(), None);
    match response.body {
        ResponseBody::Stream(mut body) => {
            while let Some(item) = body.next().await {
                match item {
                    Ok(frame) => frames.push(String::from_utf8(frame.to_vec()).unwrap()),
                    Err(error) => failure = Some(error),
                }
            }
        }
        ResponseBody::Buffered(body) => frames.push(String::from_utf8(body.to_vec()).unwrap()),
    }
    Ok((frames, failure))
}

const SPLIT_CREATED: &str = "event: response.created\ndata: {\"type\":\"response.created\",\"response\":{\"id\":\"resp_split\",\"model\":\"gpt-5.4\"}}\n\n";
/// Go's `codexOverloadEvent`: an overload that fails the attempt over during bootstrap.
const SPLIT_OVERLOAD: &str = "event: error\ndata: {\"type\":\"error\",\"error\":{\"type\":\"service_unavailable_error\",\"code\":\"server_is_overloaded\",\"message\":\"Our servers are currently overloaded. Please try again later.\",\"param\":null},\"sequence_number\":2}\n\n";

/// Go's `codex_executor_sse_events_test.go` (7795de19) for events whose data spans several
/// `data:` lines: a split event reads as its one-line form, stream and non-stream, with
/// and without bootstrap buffering, and an invalid split stays invalid.
#[tokio::test]
async fn split_sse_events_read_as_one_event() {
    let completed = r#"{"type":"response.completed","response":{"id":"resp_split","object":"response","model":"gpt-5.4","status":"completed","output":[{"id":"msg_1","type":"message","role":"assistant","content":[{"type":"output_text","text":"hi"}]}],"usage":{"input_tokens":5,"output_tokens":1,"total_tokens":6}}}"#;
    let cut = completed.find(r#""output":"#).unwrap();
    let delta = "event: response.output_text.delta\ndata: {\"type\":\"response.output_text.delta\",\"item_id\":\"msg_1\",\"output_index\":0,\"content_index\":0,\"delta\":\"hi\"}\n\n";
    let one_line = format!("{SPLIT_CREATED}{delta}event: response.completed\ndata: {completed}\n\n");
    let split = format!(
        "{SPLIT_CREATED}{delta}event: response.completed\ndata: {}\ndata: {}\n\n",
        &completed[..cut],
        &completed[cut..]
    );
    for buffering in [false, true] {
        // TestCodexExecuteStreamTreatsAnEventSplitAcrossDataLinesAsOneEvent
        let want = through_sse(&one_line, true, buffering).await.unwrap();
        let got = through_sse(&split, true, buffering).await.unwrap();
        assert!(want.1.is_none() && want.0.last().unwrap().contains("response.completed"));
        assert_eq!(got.0, want.0, "buffering={buffering}");
        assert!(got.1.is_none());
        // TestCodexExecuteStreamMinimalReproFromIssue6575 and
        // TestCodexExecuteStreamAssemblesSplitEventsWithAndWithoutBootstrapBuffering
        for sse in [
            "event: response.created\ndata: {\"type\":\"response.created\",\"response\":{\"id\":\"r\",\"status\":\"in_progress\",\"output\":[]}}\n\nevent: response.completed\ndata: {\"type\":\"response.completed\",\"response\":{\"id\":\"r\",\"status\":\"completed\",\ndata: \"output\":[]}}\n\n".to_owned(),
            format!("{SPLIT_CREATED}data: {{\"type\":\"response.in_progress\",\"response\":{{\"id\":\"resp_split\",\"output\":[\ndata: {{\"x\":1}},\ndata: {{\"x\":1}}]}}}}\n\ndata: {{\"type\":\"response.completed\",\"response\":{{\"id\":\"resp_split\",\"status\":\"completed\",\ndata: \"output\":[]}}}}\n\n"),
        ] {
            let (frames, failure) = through_sse(&sse, true, buffering).await.unwrap();
            assert!(failure.is_none(), "{failure:?}");
            assert!(!frames.iter().any(|f| f.contains("data: {\"x\":1},")), "{frames:?}");
            let last = frames.last().unwrap().trim_end();
            assert!(last.contains("\"response.completed\""), "{last}");
            let data = last.lines().find_map(|l| l.strip_prefix("data: ")).unwrap();
            assert!(serde_json::from_str::<Value>(data).is_ok(), "{last}");
        }
    }

    // TestCodexExecuteNonStreamReadsAnEventSplitAcrossDataLines, CRLF included.
    let sse = format!(
        "event: response.completed\r\ndata: {}\r\ndata: {}\r\n\r\n",
        &completed[..cut],
        &completed[cut..]
    );
    let (frames, failure) = through_sse(&sse, false, false).await.unwrap();
    assert!(failure.is_none());
    let body: Value = serde_json::from_str(&frames[0]).unwrap();
    assert_eq!(body["id"], "resp_split", "{body}");
    assert_eq!(body["output"].as_array().unwrap().len(), 1, "{body}");
}

/// Go's bootstrap budget tests for split events (7795de19): a split handshake that passes
/// the 48-line or 1 MiB budget while it is being collected releases the stream, so the
/// overload after it arrives in-stream instead of failing over, and the lines before the
/// failure in its event are delivered.
#[tokio::test]
async fn split_sse_events_spend_the_bootstrap_budget() {
    let open = "data: {\"type\":\"response.in_progress\",\"response\":{\"id\":\"resp_split\",\"items\":[\n";
    // TestCodexExecuteStreamSplitHandshakeExceedingLineBudgetReleasesBootstrapWithoutFailover
    let over_lines = format!(
        "{SPLIT_CREATED}{open}{}data: ]}}}}\n\n{SPLIT_OVERLOAD}",
        "data: \n".repeat(50)
    );
    // TestCodexExecuteStreamEventExceedsBudgetWhileAssemblingReleasesStream: the event
    // reaches 48 lines while collected, and the overload follows without a blank line.
    let no_blank = format!(
        "{SPLIT_CREATED}{open}{}data: {{\"key_at_boundary\":123}},\ndata: {{\"key_after_boundary\":456}}\ndata: ]}}}}\n{SPLIT_OVERLOAD}",
        "data: \n".repeat(43)
    );
    // TestCodexExecuteStreamMixedFramingCommentInsideSplitEventCountsBudget
    let comments = format!(
        "data: {{\"type\":\"response.in_progress\",\"items\":[\n{}: keepalive-1\n: keepalive-2\n{SPLIT_OVERLOAD}data: ]}}}}\n\n",
        "data: \n".repeat(46)
    );
    // TestCodexExecuteStreamEventExceedsByteBudgetWhileAssemblingReleasesStream
    let large = format!("data: \"{}\",\n", "x".repeat(250 * 1024));
    let over_bytes = format!(
        "data: {{\"type\":\"response.in_progress\",\"items\":[\n{}data: \"last\"]}}\n\n{SPLIT_OVERLOAD}",
        large.repeat(5)
    );
    // An overload that precedes a long split payload in its event decides first.
    let overload_first = format!(
        "{SPLIT_CREATED}{}\n{open}{}data: ]}}}}\n\n",
        SPLIT_OVERLOAD.trim_end(),
        "data: \n".repeat(60)
    );
    let error = through_sse(&overload_first, true, true).await.expect_err("fails over");
    assert_eq!(error.status, 503, "{error:?}");
    // Control: a split handshake within the budget still lets the overload fail over.
    let within = format!(
        "{SPLIT_CREATED}{open}{}data: ]}}}}\n{SPLIT_OVERLOAD}",
        "data: \n".repeat(10)
    );
    let error = through_sse(&within, true, true).await.expect_err("fails over");
    assert_eq!(error.status, 503, "{error:?}");
    // Each delivered line spends the budget before the next is read, and a handshake may
    // fill it exactly (outcomes recorded from Go v8.0.35): 47 comments and the overload's
    // `event:` line are 48 held lines; one comment of raw plus translated bytes equal to
    // 1 MiB is held, so the overload after it fails over.
    let overload_data = SPLIT_OVERLOAD.strip_prefix("event: error\n").unwrap();
    let comment = |len: usize| format!(": {}\n", "p".repeat(len - 2));
    // A completed 40-line split event, its blank line and `n` comments.
    let split_40 = |n: usize| {
        format!(
            "data: {{\"type\":\"response.in_progress\",\"items\":[\n{}data: ]}}\n\n{}{SPLIT_OVERLOAD}",
            "data: \n".repeat(38),
            ": k\n".repeat(n)
        )
    };
    for (name, sse) in [
        ("47 comments", format!("{}{SPLIT_OVERLOAD}", ": k\n".repeat(47))),
        ("40 split lines and 6 comments", split_40(6)),
        ("1 MiB exactly", format!("{}{overload_data}", comment(512 * 1024))),
    ] {
        let error = through_sse(&sse, true, true)
            .await
            .expect_err(&format!("{name}: fails over"));
        assert_eq!(error.status, 503, "{name}: {error:?}");
    }
    // Go sends a delta as read, which starts the stream before the overload after it in
    // the same event is seen.
    let delta_first =
        format!("{SPLIT_CREATED}data: {{\"type\":\"response.output_text.delta\",\"delta\":\"hi\"}}\n{SPLIT_OVERLOAD}");
    // A split handshake that completes over the byte budget (600 KB read plus 600 KB
    // translated) releases the stream before the overload after it is read.
    let completed_over_bytes = format!(
        "data: {{\"type\":\"response.in_progress\",\"response\":{{\"model\":\"gpt-5.4\"}},\"pad\":\"{}\",\ndata: \"n\":0}}\n{SPLIT_OVERLOAD}",
        "x".repeat(600_000)
    );
    let comments_48 = format!("{}{SPLIT_OVERLOAD}", ": k\n".repeat(48));
    let comments_49 = format!("{}{SPLIT_OVERLOAD}", ": k\n".repeat(49));
    let over_one_mib = format!("{}{overload_data}", comment(512 * 1024 + 1));
    let split_40_and_7 = split_40(7);
    for (name, sse) in [
        ("completed over the byte budget", &completed_over_bytes),
        ("48 comments", &comments_48),
        ("49 comments", &comments_49),
        ("1 MiB and one byte", &over_one_mib),
        ("40 split lines and 7 comments", &split_40_and_7),
        ("delta first", &delta_first),
        ("line budget", &over_lines),
        ("no blank line", &no_blank),
        ("comments", &comments),
        ("byte budget", &over_bytes),
    ] {
        let (frames, failure) = through_sse(sse, true, true)
            .await
            .unwrap_or_else(|e| panic!("{name}: the attempt failed over: {e:?}"));
        assert!(failure.is_some(), "{name}: the failure arrives in-stream");
        if name == "delta first" {
            assert!(frames.iter().any(|f| f.contains("\"delta\":\"hi\"")), "{frames:?}");
        }
        if name == "no blank line" {
            let progress = frames.iter().find(|f| f.contains("response.in_progress")).unwrap();
            let data = progress.lines().find_map(|l| l.strip_prefix("data: ")).unwrap();
            let value: Value = serde_json::from_str(data).unwrap();
            assert_eq!(
                value["response"]["items"],
                serde_json::json!([{"key_at_boundary": 123}, {"key_after_boundary": 456}])
            );
        }
    }
}

/// Go `TestCodexExecutorReplayedWebSearchCallHistoryEnsuresWebSearchTool_Issue6770`
/// (e7d4c731) through the executor: the bodies sent on the stream, non-stream and compact
/// paths, and the shaped WebSocket body, match Go's byte for byte, which pins where the
/// web search tool lands beside the image tool and the `instructions` default.
#[tokio::test]
async fn replayed_web_search_history_matches_go() {
    let go: Value = serde_json::from_str(include_str!("../tests/fixtures/codex_web_search_history_go.json")).unwrap();
    let executor = executor();
    for case in go["executor"].as_array().unwrap() {
        let name = case["name"].as_str().unwrap();
        let call = case["call"].as_str().unwrap();
        let mut attributes = serde_json::json!({"api_key": "test"});
        if let Some(plan) = case["plan"].as_str() {
            attributes["plan_type"] = plan.into();
        }
        let mut headers = serde_json::json!({});
        if case["lite_header"] == true {
            headers["x-openai-internal-codex-responses-lite"] = "true".into();
        }
        let synthetic = serde_json::json!({
            "source": "openai-response",
            "model": "gpt-6.1-sol",
            "payload": case["payload"],
            "stream": call != "non_stream" && call != "compact",
            "alt": if call == "compact" { Value::from("responses/compact") } else { Value::Null },
            "headers": headers,
            "attributes": attributes,
        });
        let cfg = Config::parse(if case["no_image"] == true {
            "disable-image-generation: true\n"
        } else {
            "{}"
        })
        .unwrap();
        let sent = if call == "websocket" {
            let credential = credential(&synthetic, "http://127.0.0.1:1");
            request::shape(
                &request(&synthetic),
                &View::new(&credential),
                &Settings::from(&cfg),
                Call::Websocket,
            )
            .unwrap()
            .body
        } else {
            let mock = Mock::start().await;
            let (path, content_type, body) = if call == "compact" {
                (
                    "/responses/compact",
                    "application/json",
                    r#"{"id":"resp_compact_1","object":"response.compaction","usage":{"input_tokens":10,"output_tokens":2,"total_tokens":12}}"#,
                )
            } else {
                (
                    "/responses",
                    "text/event-stream",
                    "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_1\",\"object\":\"response\",\"status\":\"completed\",\"output\":[],\"usage\":{\"input_tokens\":10,\"output_tokens\":5,\"total_tokens\":15}}}\n\n",
                )
            };
            mock.script(
                path,
                vec![Reply {
                    status: 200,
                    headers: vec![("content-type".into(), content_type.into())],
                    body: body.into(),
                }],
            );
            let credential = credential(&synthetic, &mock.url);
            if let ResponseBody::Stream(mut stream) = executor
                .execute(&credential, request(&synthetic), &cfg)
                .await
                .unwrap()
                .body
            {
                while stream.next().await.is_some() {}
            }
            String::from_utf8(mock.take().pop().unwrap().body.to_vec()).unwrap()
        };
        assert_eq!(sent, case["sent"].as_str().unwrap(), "{name}");
    }
}

/// Sign in with ChatGPT (7b0319fb): Go v8.0.35's upstream body and headers for a
/// `use_siwc` credential on every call, and its missing-token error; see
/// tests/reference/codex_siwc.
#[tokio::test]
async fn siwc_requests_match_go() {
    let go: Value = serde_json::from_str(include_str!("../tests/fixtures/codex_siwc_go.json")).unwrap();
    let executor = executor();
    for case in go["cases"].as_array().unwrap() {
        let name = case["name"].as_str().unwrap();
        let call = case["call"].as_str().unwrap();
        let mut metadata = case["metadata"].clone();
        metadata["type"] = "codex".into();
        let synthetic = serde_json::json!({
            "source": "openai-response",
            "model": case["model"],
            "payload": case["payload"],
            "stream": call == "stream" || call == "websocket",
            "alt": if call == "compact" { Value::from("responses/compact") } else { Value::Null },
            "headers": case["headers"],
            "attributes": case["attributes"],
            "metadata": metadata,
        });
        let cfg = Config::parse(case["config"].as_str().unwrap_or("{}")).unwrap();
        let mock = Mock::start().await;
        let credential = credential(&synthetic, &mock.url);
        // The recorder ran on linux/amd64; the User-Agent names the host's Go platform.
        let mut sent_headers = case["sent_headers"].clone();
        if let Some(ua) = sent_headers.get_mut("user-agent") {
            let host = format!("({}; {})", crate::kimi_http::go_os(), crate::kimi_http::go_arch());
            ua[0] = ua[0].as_str().unwrap().replace("(linux; amd64)", &host).into();
        }
        let go_upstream = serde_json::json!({
            "path": case["path"],
            "body": case["body"],
            "headers": sent_headers,
        });
        if call == "websocket" {
            let req = request(&synthetic);
            let view = View::new(&credential);
            let settings = Settings::scoped(&cfg, &view);
            let shaped = request::shape(&req, &view, &settings, Call::Websocket).unwrap();
            let (body, cache) = request::prompt_cache(&req, shaped.body, Some("exec-session"), true);
            let body = crate::codex_ws::turn_body(&view, &req, &settings, &shaped.finalize, body);
            assert_eq!(body, case["body"].as_str().unwrap(), "{name}: body");
            let headers = request::ws_headers(
                &view,
                &settings,
                &req.headers,
                &body,
                &request::base_model(&req.model),
                cache.as_deref(),
                false,
            );
            let mut actual: Vec<(String, String)> = headers
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_str().unwrap().to_owned()))
                .collect();
            actual.sort();
            let mut expected: Vec<(String, String)> = go_upstream["headers"]
                .as_object()
                .unwrap()
                .iter()
                .map(|(k, v)| (k.clone(), v[0].as_str().unwrap().to_owned()))
                .collect();
            expected.sort();
            assert_eq!(actual, expected, "{name}: headers");
            continue;
        }
        if call == "image" {
            let sse = concat!(
                "data: {\"type\":\"response.output_item.done\",\"output_index\":0,\"item\":{\"type\":\"image_generation_call\",\"id\":\"ig_0\",\"status\":\"completed\",\"result\":\"SU1HMQ==\",\"output_format\":\"png\"}}\n\n",
                "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_img\",\"created_at\":1700000000,\"status\":\"completed\",\"output\":[],\"usage\":{\"input_tokens\":1,\"output_tokens\":2,\"total_tokens\":3}}}\n\n",
            );
            mock.script(
                "/responses",
                vec![Reply {
                    status: 200,
                    headers: vec![("content-type".into(), "text/event-stream".into())],
                    body: sse.into(),
                }],
            );
            let path = "/v1/images/generations";
            let mut req = request(&synthetic);
            (req.source_format, req.response_format, req.request_path) = (Format::OpenAI, Format::OpenAI, path.into());
            executor.images(&credential, req, path, &cfg).await.unwrap();
            assert_same_upstream(name, &mock.take().pop().unwrap(), &go_upstream);
            continue;
        }
        let (path, content_type, reply) = if call == "compact" {
            (
                "/responses/compact",
                "application/json",
                r#"{"id":"resp_compact_1","object":"response.compaction","usage":{"input_tokens":10,"output_tokens":2,"total_tokens":12}}"#,
            )
        } else {
            (
                "/responses",
                "text/event-stream",
                "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_1\",\"object\":\"response\",\"status\":\"completed\",\"output\":[],\"usage\":{\"input_tokens\":10,\"output_tokens\":5,\"total_tokens\":15}}}\n\n",
            )
        };
        mock.script(
            path,
            vec![Reply {
                status: 200,
                headers: vec![("content-type".into(), content_type.into())],
                body: reply.into(),
            }],
        );
        match executor.execute(&credential, request(&synthetic), &cfg).await {
            Ok(res) => {
                if let ResponseBody::Stream(mut stream) = res.body {
                    while stream.next().await.is_some() {}
                }
                assert_same_upstream(name, &mock.take().pop().unwrap(), &go_upstream);
            }
            Err(error) => {
                assert_eq!(error.status as i64, case["status"].as_i64().unwrap(), "{name}: status");
                assert_eq!(
                    String::from_utf8_lossy(&error.body),
                    case["error"].as_str().unwrap(),
                    "{name}: error"
                );
                assert!(mock.take().is_empty(), "{name}: nothing may reach upstream");
            }
        }
    }
    // Without a `base_url` (the cases above dial a mock): Go's `defaultOpenAISIWCBaseURL`.
    let production = Credential::from_file(
        Path::new("/fake"),
        Path::new("/fake/c.json"),
        serde_json::json!({"type": "codex", "use_siwc": true, "access_token": "outer-at",
                           "siwc": {"access_token": "siwc-at"}})
        .as_object()
        .unwrap()
        .clone(),
    )
    .unwrap();
    let view = View::new(&production);
    assert_eq!((view.base_url, view.token), ("https://api.openai.com/v1", "siwc-at"));
}

/// Sign in with ChatGPT refresh (Go `refreshLocked`): the `siwc` partition refreshes on
/// its own 5-minute lead, a rejected token refreshes the partition requests use, a
/// management refresh both, and one partition failing keeps the other's result.
#[tokio::test]
async fn siwc_partition_refreshes_like_go() {
    use crate::codex_testkit::json;
    const OUTER: &str = "/oauth/token";
    const SIWC: &str = "/api/accounts/oauth/token";
    let mock = Mock::start().await;
    let oauth = CodexOAuth::with_endpoints(wreq::Client::new(), &format!("{}{OUTER}", mock.url), "", "")
        .with_siwc_token_url(&format!("{}{SIWC}", mock.url))
        .with_retry_delay(std::time::Duration::from_millis(1));
    let executor = CodexExecutor::with_client(wreq::Client::new(), oauth);
    let cfg = Config::default();
    let at = |minutes: i64| (Utc::now() + chrono::TimeDelta::minutes(minutes)).to_rfc3339();
    // Unique refresh tokens per case: results are single-flighted and retained.
    let cred = |id: &str, use_siwc: bool, siwc_expires: &str| {
        Credential::from_file(
            Path::new("/fake"),
            Path::new("/fake/c.json"),
            serde_json::json!({
                "type": "codex", "use_siwc": use_siwc,
                "access_token": "outer-at", "refresh_token": format!("outer-{id}"), "expired": at(5 * 24 * 60),
                "siwc": {"access_token": "siwc-at", "refresh_token": format!("siwc-{id}"), "client_id": "client-1",
                         "device_id": "dev-1", "expired": siwc_expires, "custom": "kept"},
            })
            .as_object()
            .unwrap()
            .clone(),
        )
        .unwrap()
    };
    let outer_ok = || {
        json(
            200,
            r#"{"access_token":"outer-new","refresh_token":"outer-rt-new","id_token":"","expires_in":3600}"#,
        )
    };
    let siwc_ok = || {
        json(
            200,
            r#"{"access_token":"siwc-new","refresh_token":"siwc-rt-new","scope":"openid chatgpt.tokens.use.direct","expires_in":3600}"#,
        )
    };
    let paths = |sent: Vec<Captured>| sent.into_iter().map(|c| c.path).collect::<Vec<_>>();

    // The 5-minute lead: due at four minutes left, not at six.
    assert!(!executor.needs_prepare(&cred("a", true, &at(6)), &cfg));
    let due = cred("b", true, &at(4));
    assert!(executor.needs_prepare(&due, &cfg));
    mock.script(SIWC, vec![siwc_ok()]);
    let patch = executor.prepare(&due, &cfg).await.unwrap();
    let sent = mock.take();
    assert_eq!(paths(sent.clone()), [SIWC], "only the due partition refreshes");
    assert_eq!(
        String::from_utf8_lossy(&sent[0].body),
        "client_id=client-1&grant_type=refresh_token&refresh_token=siwc-b&resource=https%3A%2F%2Fapi.openai.com%2Fv1"
    );
    assert_eq!(patch.set.keys().collect::<Vec<_>>(), ["siwc"]);
    let siwc = &patch.set["siwc"];
    assert_eq!(
        (&siwc["access_token"], &siwc["refresh_token"], &siwc["client_id"]),
        (&"siwc-new".into(), &"siwc-rt-new".into(), &"client-1".into())
    );
    assert_eq!((&siwc["device_id"], &siwc["custom"]), (&"dev-1".into(), &"kept".into()));

    // A rejected token: the partition requests use, whichever is due otherwise.
    mock.script(SIWC, vec![siwc_ok()]);
    executor.refresh_now(&cred("c", true, &at(60)), &cfg).await.unwrap();
    assert_eq!(paths(mock.take()), [SIWC]);
    mock.script(OUTER, vec![outer_ok()]);
    let patch = executor.refresh_now(&cred("d", false, &at(60)), &cfg).await.unwrap();
    assert_eq!(paths(mock.take()), [OUTER]);
    assert!(!patch.set.contains_key("siwc"));

    // A management refresh: both, outer first.
    mock.script(OUTER, vec![outer_ok()]);
    mock.script(SIWC, vec![siwc_ok()]);
    let patch = executor.refresh_all(&cred("e", false, &at(60)), &cfg).await.unwrap();
    assert_eq!(paths(mock.take()), [OUTER, SIWC]);
    assert_eq!(
        (&patch.set["access_token"], &patch.set["siwc"]["access_token"]),
        (&"outer-new".into(), &"siwc-new".into())
    );

    // One partition failing keeps the other's refresh; both failing returns the outer error.
    mock.script(OUTER, vec![json(500, "{}"); 3]);
    mock.script(SIWC, vec![siwc_ok()]);
    let patch = executor.refresh_all(&cred("f", false, &at(60)), &cfg).await.unwrap();
    assert_eq!(patch.set.keys().collect::<Vec<_>>(), ["siwc"]);
    mock.take();
    mock.script(OUTER, vec![outer_ok()]);
    mock.script(SIWC, vec![json(500, "{}"); 3]);
    let patch = executor.refresh_all(&cred("k", false, &at(60)), &cfg).await.unwrap();
    assert_eq!(patch.set["access_token"], "outer-new");
    assert!(!patch.set.contains_key("siwc"));
    assert_eq!(mock.take().len(), 4);
    // A retry right after: the outer success is retained, the SIWC failure is not (Go's
    // singleflight forgets it), so only the SIWC endpoint is called again.
    mock.script(SIWC, vec![siwc_ok()]);
    let patch = executor.refresh_all(&cred("k", false, &at(60)), &cfg).await.unwrap();
    assert_eq!(paths(mock.take()), [SIWC]);
    assert_eq!(patch.set["siwc"]["access_token"], "siwc-new");
    mock.script(OUTER, vec![json(500, "{}"); 3]);
    mock.script(SIWC, vec![json(500, "{}"); 3]);
    let error = executor
        .refresh_all(&cred("g", false, &at(60)), &cfg)
        .await
        .unwrap_err();
    assert!(
        String::from_utf8_lossy(&error.body).starts_with("token refresh failed after 3 attempts"),
        "{error:?}"
    );
    assert_eq!(mock.take().len(), 6);

    // refresh_token_reused stops the retries, and only the OAuth code is kept.
    mock.script(
        SIWC,
        vec![json(400, r#"{"error":"refresh_token_reused","echo":"secret"}"#)],
    );
    let error = executor.refresh_now(&cred("h", true, &at(60)), &cfg).await.unwrap_err();
    assert_eq!(mock.take().len(), 1);
    assert_eq!((error.status, error.scope), (401, FailureScope::Credential));
    assert_eq!(
        String::from_utf8_lossy(&error.body),
        "OpenAI OAuth token request failed (400): refresh_token_reused"
    );

    // A failed scheduled refresh keeps a still-valid SIWC token in use.
    mock.script(SIWC, vec![json(500, "{}"); 3]);
    assert!(
        executor
            .prepare(&cred("i", true, &at(4)), &cfg)
            .await
            .unwrap()
            .set
            .is_empty()
    );
    mock.take();
    mock.script(SIWC, vec![json(500, "{}"); 3]);
    assert!(executor.prepare(&cred("j", true, &at(-1)), &cfg).await.is_err());
}

/// TestPayloadBarrierCodexImageFilter, HTTP half (868e137): a filter removes what the
/// executor adds late (the image tool, the empty instructions, the prompt cache key).
#[tokio::test]
async fn payload_filters_remove_late_codex_additions() {
    let executor = executor();
    let cfg = Config::parse(
        "payload:\n  filter:\n    - models: [{name: gpt-5.6-sol}]\n      params: ['tools.#(type==\"image_generation\")#', instructions, prompt_cache_key]\n",
    )
    .unwrap();
    for stream in [false, true] {
        for model in ["gpt-5.6-sol", "gpt-5.6-luna"] {
            let mock = Mock::start().await;
            mock.script(
                "/responses",
                vec![Reply {
                    status: 200,
                    headers: vec![("content-type".into(), "text/event-stream".into())],
                    body: "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_barrier\",\"status\":\"completed\",\"output\":[],\"usage\":{\"input_tokens\":0,\"output_tokens\":0,\"total_tokens\":0}}}\n\n".into(),
                }],
            );
            let mut credential = oauth_credential(&mock.url);
            credential.attributes.insert("api_key".into(), "test".into());
            credential.attributes.insert("plan_type".into(), "pro".into());
            let mut req = plain_request(stream, None);
            req.source_format = Format::OpenAIResponse;
            req.response_format = Format::OpenAIResponse;
            req.model = model.into();
            req.requested_model = model.into();
            req.body = Bytes::from_static(br#"{"input":"hello","prompt_cache_key":"injected-cache"}"#);
            req.original_body = req.body.clone();
            match executor.execute(&credential, req, &cfg).await.unwrap().body {
                ResponseBody::Stream(mut stream) => while stream.next().await.is_some() {},
                ResponseBody::Buffered(_) => {}
            }
            let sent = String::from_utf8(mock.take().pop().unwrap().body.to_vec()).unwrap();
            let image = gjson::get(&sent, r#"tools.#(type=="image_generation")"#).exists();
            assert_eq!(image, model == "gpt-5.6-luna", "{model} stream={stream}: {sent}");
            if model == "gpt-5.6-sol" {
                assert!(!gjson::get(&sent, "instructions").exists(), "{sent}");
                assert!(!gjson::get(&sent, "prompt_cache_key").exists(), "{sent}");
            }
        }
    }
}

/// Go 868e137 gives CountTokens the payload barrier: the rules run on the shaped body
/// before the tokenizer, so a configured `input` changes the count.
#[test]
fn count_tokens_sees_the_payload_rules() {
    let count = |config: &str| {
        let mut req = plain_request(false, None);
        req.operation = Operation::CountTokens;
        req.body = Bytes::from_static(br#"{"model":"gpt-5.4","input":[{"type":"message","role":"user","content":[{"type":"input_text","text":"hi"}]}]}"#);
        req.original_body = req.body.clone();
        let out = super::count_tokens(&req, &Config::parse(config).unwrap()).unwrap();
        let ResponseBody::Buffered(bytes) = out.body else {
            panic!("count answers are buffered")
        };
        let text = String::from_utf8(bytes.to_vec()).unwrap();
        gjson::get(&text, "response.usage.input_tokens").i64()
    };
    let plain = count("");
    let configured = count(
        "payload:\n  override:\n    - models: [{name: gpt-5.4}]\n      params: {input.0.content.0.text: one two three four five six seven eight nine ten}\n",
    );
    assert!(plain > 0);
    assert!(configured > plain + 5, "{plain} -> {configured}");
}
