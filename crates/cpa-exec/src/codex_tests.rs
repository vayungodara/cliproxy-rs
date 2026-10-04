//! Executor parity: each case in `tests/fixtures/codex_go.json` ran through Go's
//! `CodexExecutor` against a scripted loopback upstream. The Rust executor gets the same
//! request and upstream reply; the upstream request and the client-visible output must match.

use super::*;
use crate::codex_testkit::{Captured, GO, Mock, Reply, Tap, Wiretap};
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
        resolved_model: None,
        usage: Default::default(),
        request_path: String::new(),
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

/// Usage records see the upstream side, as Go's reporter does: the translated request
/// (`SetTranslatedReasoningEffort`, before the prompt-cache key is added) and every
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
        let mut sent = case["upstream"]["body"].as_str().unwrap().as_bytes().to_vec();
        cpa_common::json::delete(&mut sent, "prompt_cache_key");
        assert_eq!(
            reports[0],
            ("request", upstream_format, String::from_utf8(sent).unwrap()),
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
        let mut recorded: Vec<(String, String)> = headers.iter().map(|(k, v)| (k.to_lowercase(), v.clone())).collect();
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
