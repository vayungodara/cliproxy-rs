use super::*;
use cpa_core::exec::Caller;
use cpa_core::gostr::GoStr;
use serde_json::Value;
use std::path::Path;
use std::time::Duration;

thread_local! {
    /// The clock requests prepared on this test thread read, when set.
    static CLOCK: std::cell::Cell<Option<chrono::DateTime<chrono::Utc>>> = const { std::cell::Cell::new(None) };
    /// The date the currentDate reminder reads on this test thread, when pinned.
    static TODAY: std::cell::RefCell<Option<String>> = const { std::cell::RefCell::new(None) };
}

/// The instant [`set_clock`] set for this thread.
pub(super) fn clock() -> Option<chrono::DateTime<chrono::Utc>> {
    CLOCK.get()
}

/// Sets the clock requests prepared on this thread read, as Go's tests do with
/// `claudeCodeCurrentTimeFunc`, until a [`Pins`] guard drops. `#[tokio::test]` runs the
/// executor on the test's own thread.
fn set_clock(now: Option<&str>) {
    CLOCK.set(now.map(|now| chrono::DateTime::parse_from_rfc3339(now).unwrap().to_utc()));
}

/// The local date [`pin_today`] pinned for this thread.
pub(super) fn pinned_today() -> Option<String> {
    TODAY.with_borrow(Clone::clone)
}

/// Pins the date itself, for replays of a Go run that recorded only the date its clock
/// gave in the credential's zone, until the returned guard drops.
pub(super) fn pin_today(date: Option<&str>) -> Pins {
    TODAY.with_borrow_mut(|today| *today = date.map(str::to_owned));
    Pins
}

/// Releases this thread's clock and date pins when it drops, so a test that panics
/// leaves neither behind.
pub(super) struct Pins;

impl Drop for Pins {
    fn drop(&mut self) {
        CLOCK.set(None);
        TODAY.with_borrow_mut(|today| *today = None);
    }
}

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
        stream_payloads: false,
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
            provider: String::new(),
        },
    }
}

#[tokio::test]
async fn pipeline_reproduces_go_upstream_captures() {
    let captures: Value = serde_json::from_str(include_str!("testdata/go_captures.json")).unwrap();
    let executor = ClaudeExecutor::with_client(wreq::Client::new(), DEFAULT_BASE_URL);
    let credential = harness_credential();
    let cfg = Config::parse("").unwrap();
    for case in captures["cases"].as_array().unwrap() {
        let name = case["name"].as_str().unwrap();
        let req = enrich(request(case));
        let mut ctx = Ctx::new(&executor, &credential, &req, &cfg, Default::default());
        ctx.today = "2026-10-02".into();
        let translated =
            translate::request(&req, ctx.codex, &ctx.base_model, ctx.is_compat, ctx.tool_search()).unwrap();
        let prepared = if req.operation == Operation::CountTokens {
            let baseline = translate::count_original(
                &req,
                &translated,
                ctx.codex,
                &ctx.base_model,
                ctx.is_compat,
                ctx.tool_search(),
            )
            .unwrap();
            ctx.prepare_count(&req, &translated, &baseline).await.unwrap()
        } else {
            let original = translate::original(
                &req,
                &translated,
                ctx.codex,
                &ctx.base_model,
                ctx.is_compat,
                ctx.tool_search(),
            )
            .unwrap();
            ctx.prepare_messages(&req, &translated, &original, req.stream)
                .await
                .unwrap()
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
        stream_payloads: false,
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
            provider: String::new(),
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

/// count_tokens checks `default` rules against the client's original request, not the
/// working body (868e137): a default for a field only the original sets is not added.
#[tokio::test]
async fn count_defaults_read_the_original_request() {
    let mut credential = Credential::from_file(
        Path::new("/fake"),
        Path::new("/fake/claude.json"),
        serde_json::json!({"type":"claude","access_token":"sk-ant-oat-fake"})
            .as_object()
            .unwrap()
            .clone(),
    )
    .unwrap();
    // A custom origin counts locally, so nothing is sent.
    credential
        .attributes
        .insert("base_url".into(), "http://127.0.0.1:9".into());
    let working = br#"{"model":"claude","messages":[{"role":"user","content":"Hello."}]}"#;
    let count = |config: &'static str, original: &'static [u8]| {
        let executor = ClaudeExecutor::with_client(wreq::Client::new(), DEFAULT_BASE_URL);
        let credential = credential.clone();
        async move {
            let req = ExecRequest {
                operation: Operation::CountTokens,
                source_format: Format::Claude,
                response_format: Format::Claude,
                stream_payloads: false,
                requested_model: "claude".into(),
                model: "claude".into(),
                original_body: Bytes::from_static(original),
                body: Bytes::from_static(working),
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
                    provider: String::new(),
                },
            };
            let response = executor
                .execute(&credential, req, &Config::parse(config).unwrap())
                .await
                .unwrap();
            let ResponseBody::Buffered(body) = response.body else {
                panic!("count is buffered")
            };
            String::from_utf8(body.to_vec()).unwrap()
        }
    };
    let rule = "payload:\n  default:\n    - models: [{name: claude}]\n      params: {system: \"A configured system prompt that adds tokens\"}\n";
    let original = br#"{"model":"claude","system":"x","messages":[{"role":"user","content":"Hello."}]}"#;
    let plain = count("", working).await;
    let defaulted = count(rule, working).await;
    assert_ne!(plain, defaulted, "the default applies when neither body sets system");
    assert_eq!(count(rule, original).await, plain, "the original sets system");
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
            stream_payloads: false,
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
                provider: String::new(),
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
    finish(
        decode_upstream(upstream).await,
        fast,
        ctx.settings.model_level_cooling,
        &Default::default(),
    )
    .await
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

const GO_EXECUTOR: &str = include_str!("testdata/go_executor.json");

#[tokio::test]
async fn executor_scenarios_match_go() {
    // Sequenced through execute(): compat_replay_sequence_matches_go; config keys:
    // m1_0041_to_m1_0056_config_keys_match_go.
    scenarios_match_go(GO_EXECUTOR, "all", |name| {
        !name.starts_with("replay-")
            && !is_config_scenario(name)
            && !FOLLOWS_V8_0_27.contains(&name)
            && !FOLLOWS_V8_0_35.contains(&name)
    })
    .await;
}

/// 6fecc6e scenarios with payload rules: the rules now run once, last (868e137), and
/// the v8.0.35 fixture holds Go's answer.
const FOLLOWS_V8_0_35: &[&str] = &[
    "payload-model-to-fable",
    "payload-model-to-legacy",
    "payload-declassify-probe",
    "payload-make-probe-fable",
    "payload-default-reads-original",
    "payload-filter-fable",
];

/// Requests without payload rules that cloaking turns into a probe or a helper, or out
/// of one, and image-tool stripping and `prompt_cache_options` handling, which now run
/// after cloaking. cliproxy-rs before 868e137 sent each of these differently.
const NO_RULES_V8_0_35: &[&str] = &[
    "noroles-quota-probe-with-system",
    "noroles-legacy-title-helper",
    "noroles-hi-becomes-probe",
    "noroles-image-tool-last",
    "noroles-image-tool-only-probe",
    "noroles-duplicate-prompt-cache-options",
    "noroles-duplicate-prompt-cache-options-explicit",
    "noroles-duplicate-thinking",
    "noroles-duplicate-tool-choice",
    "noroles-duplicate-cache-control",
    "noroles-duplicate-metadata",
    // Go's IsArray guards and ForEach walks on non-array messages, content and tools.
    "noroles-object-messages-system",
    "noroles-object-messages-user",
    "noroles-object-messages-foreach",
    "noroles-object-messages-timing",
    "noroles-object-messages-title-schema",
    "noroles-object-messages-advisor",
    "noroles-object-content-inline-tools",
    "noroles-object-tools-advisor",
    // strings.EqualFold(strings.TrimSpace(role), "system"), len(props.Map()), EqualFold.
    "noroles-mid-system-spaced-role",
    "noroles-title-duplicate-props",
    "noroles-fold-speed-display",
];

/// 868e137 through Go v8.0.35's executor (scenarios-v8.0.35.json): payload rules see
/// the fully prepared body and nothing repairs their result.
#[tokio::test]
async fn v8_0_35_payload_barrier_scenarios_match_go() {
    let fixture = include_str!("testdata/go_executor_v8.0.35.json");
    assert!(fixture.contains(r#""source": "CLIProxyAPI v8.0.35 "#));
    for name in FOLLOWS_V8_0_35.iter().chain(NO_RULES_V8_0_35) {
        assert!(fixture.contains(&format!(r#""name": "{name}""#)), "{name}");
    }
    scenarios_match_go(fixture, "v8.0.35", |_| true).await;
}

/// `upstream.claude.tool-search` on Responses requests (Go v8.0.35): native tool search
/// on Anthropic's origin and on keys with `native-tool-search`, the pre-tool_search
/// translation elsewhere, for legacy models and with the switch off.
#[tokio::test]
async fn tool_search_scenarios_match_go() {
    let fixture = include_str!("testdata/go_executor_tool_search.json");
    assert!(fixture.contains(r#""source": "CLIProxyAPI v8.0.35 "#));
    scenarios_match_go(fixture, "tool-search", |_| true).await;
}

/// Go `TestClaudeExecutor_SupportsNativeToolSearch`: which credentials take Codex
/// tool_search natively, while the switch is on (and none while it is off).
#[test]
fn native_tool_search_support_matches_go() {
    const GATEWAY: &str = "https://gateway.example.test";
    let on = "upstream:\n  claude:\n    tool-search: true\n";
    let forced =
        format!("{on}claude-api-key:\n  - api-key: k\n    base-url: {GATEWAY}\n    native-tool-search: true\n");
    type Case<'a> = (&'a str, &'a str, &'a [(&'a str, &'a str)], &'a str, bool);
    let cases: &[Case] = &[
        ("default origin", on, &[("api_key", "k")], "claude-sonnet-4-5", true),
        (
            "explicit anthropic",
            on,
            &[("api_key", "k"), ("base_url", "https://api.anthropic.com")],
            "claude-sonnet-4-5",
            true,
        ),
        (
            "anthropic with port",
            on,
            &[("api_key", "k"), ("base_url", "https://api.anthropic.com:443/")],
            "claude-sonnet-4-5",
            true,
        ),
        (
            "plain http anthropic",
            on,
            &[("api_key", "k"), ("base_url", "http://api.anthropic.com")],
            "claude-sonnet-4-5",
            false,
        ),
        (
            "gateway",
            on,
            &[("api_key", "k"), ("base_url", GATEWAY)],
            "claude-sonnet-4-5",
            false,
        ),
        (
            "gateway forced by attribute",
            on,
            &[("api_key", "k"), ("base_url", GATEWAY), ("native_tool_search", "TRUE")],
            "claude-sonnet-4-5",
            true,
        ),
        (
            "gateway forced by config",
            &forced,
            &[("api_key", "k"), ("base_url", GATEWAY)],
            "claude-sonnet-4-5",
            true,
        ),
        (
            "legacy model on anthropic",
            on,
            &[("api_key", "k")],
            "claude-opus-4-1-20250805",
            false,
        ),
        (
            "legacy model on forced gateway",
            on,
            &[("api_key", "k"), ("base_url", GATEWAY), ("native_tool_search", "true")],
            "claude-3-7-sonnet-20250219",
            false,
        ),
        ("switch off", "", &[("api_key", "k")], "claude-sonnet-4-5", false),
    ];
    for (name, cfg, attrs, model, want) in cases {
        let mut credential = harness_credential();
        credential.attributes = attrs.iter().map(|(k, v)| ((*k).to_owned(), (*v).to_owned())).collect();
        let supports = native_tool_search(&Config::parse(cfg).unwrap());
        assert_eq!(supports(&credential, model), *want, "{name}");
    }
}

/// The same Go recordings through `execute` against a loopback upstream: the response
/// translation reads the request's tool-search switch, so a native tool_search tool_use
/// answers as a tool_search_call and a fallback one as a plain function call.
#[tokio::test]
async fn tool_search_responses_match_go() {
    let fixture: Value = serde_json::from_str(include_str!("testdata/go_executor_tool_search.json")).unwrap();
    let created = regex::Regex::new(r#""created_at":\d+"#).unwrap();
    for name in ["tool-search-native-key-non-stream", "tool-search-fallback-key"] {
        let scenario = fixture["scenarios"]
            .as_array()
            .unwrap()
            .iter()
            .find(|s| s["name"] == name)
            .unwrap();
        let reply = scenario["reply"]["body"].as_str().unwrap().to_owned();
        let router = axum::Router::new().fallback(move || {
            let reply = reply.clone();
            async move { ([(http::header::CONTENT_TYPE, "text/event-stream")], reply) }
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        let dir = std::env::temp_dir().join(format!("cpa-claude-tool-search-{name}-{}", std::process::id()));
        let text = scenario["config"]
            .as_str()
            .unwrap()
            .replace("AUTH_DIR", dir.join("auth").to_str().unwrap())
            .replace("http://gateway.invalid", &base);
        let cfg = Config::parse(&text).unwrap();
        let credential = cpa_core::config::credentials::load(&cfg)
            .into_iter()
            .find(|c| c.provider == "claude")
            .unwrap();
        let body = Bytes::from(scenario["body"].as_str().unwrap().to_owned());
        let req = ExecRequest {
            operation: Operation::Generate,
            source_format: Format::OpenAIResponse,
            response_format: Format::OpenAIResponse,
            stream_payloads: false,
            requested_model: "claude-sonnet-4-6".into(),
            model: "claude-sonnet-4-6".into(),
            original_body: body.clone(),
            body,
            stream: scenario["stream"].as_bool().unwrap(),
            alt: None,
            session: None,
            execution_session: None,
            derived_session: None,
            resolved_model: None,
            usage: Default::default(),
            request_path: String::new(),
            headers: Default::default(),
            caller: Caller {
                principal: "client-key-1".into(),
                source: "authorization",
                provider: String::new(),
            },
        };
        let executor = ClaudeExecutor::with_client(wreq::Client::new(), DEFAULT_BASE_URL);
        let output: Vec<String> = match executor.execute(&credential, req, &cfg).await.unwrap().body {
            ResponseBody::Buffered(b) => vec![String::from_utf8(b.to_vec()).unwrap()],
            ResponseBody::Stream(events) => {
                events
                    .map(|c| String::from_utf8(c.unwrap().to_vec()).unwrap())
                    .collect()
                    .await
            }
        };
        let go: Vec<String> = scenario["output"]
            .as_array()
            .unwrap()
            .iter()
            .map(|o| o.as_str().unwrap().to_owned())
            .collect();
        let mask = |v: &[String]| -> Vec<String> {
            v.iter()
                .map(|o| created.replace_all(o, r#""created_at":0"#).into_owned())
                .collect()
        };
        assert_eq!(mask(&output), mask(&go), "{name}");
    }
}

/// 6fecc6e scenarios whose outcome now follows Go v8.0.27: the v8.0.27 fixture runs
/// each of them again and holds the answer the executor gives now.
const FOLLOWS_V8_0_27: &[&str] = &[
    // 8080967: a reset past the seven-day window is a billing boundary, not a retry hint.
    "oauth-unified-429",
    "oauth-fast-unified-429",
];

/// v8.0.27 scenarios where Go keeps an API key's caller system text top-level and
/// cliproxy-rs moves it after the leading user run, as it does for every request
/// (docs/DIFFERENCES-FROM-GO.md). Rust's body is compared with Go's after
/// [`with_relocated_system`].
const RELOCATES_API_KEY_SYSTEM: &[&str] = &[
    "apikey-cloak-system-single-user",
    "apikey-cloak-system-leading-run",
    "apikey-cloak-count-system-single-user",
    "apikey-cloak-effort-directive",
];

/// Go v8.0.27's upstream body with the caller system blocks it left top-level (those
/// after the billing, Claude Code identity and Fable blocks) moved into one system
/// turn each after the leading user run (the first user turn and the user turns and
/// effort directives that follow it), where cliproxy-rs puts them for API keys too.
/// Every other byte is Go's; a body without such blocks comes back unchanged.
pub(super) fn with_relocated_system(go_body: &str) -> String {
    let system = rawjson::get(go_body, "system");
    let blocks = system.array();
    let ours = |block: &rawjson::Value| {
        let text = block.get("text");
        let text = text.str();
        text.starts_with(cloak::BILLING_PREFIX) || text == cloak::CLI_IDENTITY || text == cloak::FABLE_REPORTING
    };
    let kept = blocks.iter().take_while(|b| ours(b)).count();
    if kept == blocks.len() {
        return go_body.to_owned();
    }
    let messages = rawjson::get(go_body, "messages");
    let messages = messages.array();
    let role = |i: usize| messages[i].get("role").str().to_owned();
    let first = (0..messages.len()).find(|&i| role(i) == "user").expect("a user turn");
    let mut at = first + 1;
    while at < messages.len()
        && (role(at) == "user" || (role(at) == "system" && messages[at].get("output_config").exists()))
    {
        at += 1;
    }
    let turns: Vec<String> = blocks[kept..]
        .iter()
        .map(|b| format!(r#"{{"role":"system","content":[{}]}}"#, b.json()))
        .collect();
    let list: Vec<&str> = messages[..at]
        .iter()
        .map(|m| m.json())
        .chain(turns.iter().map(String::as_str))
        .chain(messages[at..].iter().map(|m| m.json()))
        .collect();
    let body = rawjson::set_raw(go_body, "messages", &format!("[{}]", list.join(",")));
    if kept == 0 {
        return rawjson::delete(&body, "system");
    }
    let cpa: Vec<&str> = blocks[..kept].iter().map(|b| b.json()).collect();
    rawjson::set_raw(&body, "system", &format!("[{}]", cpa.join(",")))
}

/// The prompt-cache and prompt-shaping fixes this executor follows from v8.0.27, run
/// through Go v8.0.27's executor by the same generator (scenarios-v8.0.27.json): the
/// session-pinned currentDate reminder (6d57ac9), thread breakpoint reservation and
/// continuation aliases (26e5efb, 678a925, e7b2226), explicit prompt cache options
/// (eb6a768), OAuth system placement and effort directives (14dc4e0, 2d8ec73),
/// invalid markers (be8c35f) and overage billing resets (8080967). API-key system
/// placement keeps 6fecc6e's ([`RELOCATES_API_KEY_SYSTEM`]). The scenarios run in
/// order on one executor: a date pin and a thread's aliases carry from one scenario to
/// the next, as in Go's run.
#[tokio::test]
async fn v8_0_27_cache_and_prompt_shaping_scenarios_match_go() {
    let fixture = include_str!("testdata/go_executor_v8.0.27.json");
    assert!(fixture.contains(r#""source": "CLIProxyAPI v8.0.27 "#));
    for name in FOLLOWS_V8_0_27 {
        assert!(fixture.contains(&format!(r#""name": "{name}""#)), "{name}");
    }
    scenarios_match_go(fixture, "v8.0.27", |_| true).await;
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
    scenarios_match_go(GO_EXECUTOR, "config", is_config_scenario).await;
}

async fn scenarios_match_go(fixture: &str, run: &str, selected: fn(&str) -> bool) {
    let fixture: Value = serde_json::from_str(fixture).unwrap();
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
            // The file name is the credential ID that keys the process-wide continuity
            // store. One name per run keeps a run's date pins and prompt IDs to itself:
            // the 6fecc6e and v8.0.27 fixtures send the same requests on different
            // dates, and whichever ran first used to pin its date for the other.
            std::fs::write(dir.join("auth").join(format!("fixture-{run}.json")), auth).unwrap();
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
            stream_payloads: false,
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
                provider: String::new(),
            },
        };
        let mut req = enrich(req);
        let mut ctx = Ctx::new(&executor, &credential, &req, &cfg, Default::default());
        ctx.today = scenario["date"].as_str().unwrap().into();
        if let (true, true, Some(output)) = (count, scenario["upstream"].is_null(), scenario["output"].as_array()) {
            // Counted locally: Go answered without an upstream request.
            let response = executor.count_tokens(ctx, req, false).await.unwrap();
            let ResponseBody::Buffered(body) = response.body else {
                panic!("{name}: streamed count")
            };
            assert_eq!(
                String::from_utf8_lossy(&body),
                output[0].as_str().unwrap(),
                "{name}: count"
            );
            continue;
        }
        // The stages below run inside generate() and count_tokens(), which drop
        // tool_search first; count_tokens() itself is driven above and in go_unit.
        ctx.tool_search_fallback(&mut req);
        let translated =
            translate::request(&req, ctx.codex, &ctx.base_model, ctx.is_compat, ctx.tool_search()).unwrap();
        let prepared = if count {
            let baseline = translate::count_original(
                &req,
                &translated,
                ctx.codex,
                &ctx.base_model,
                ctx.is_compat,
                ctx.tool_search(),
            )
            .unwrap();
            ctx.prepare_count(&req, &translated, &baseline).await
        } else {
            // generate(): translated clients always stream upstream.
            let original = translate::original(
                &req,
                &translated,
                ctx.codex,
                &ctx.base_model,
                ctx.is_compat,
                ctx.tool_search(),
            )
            .unwrap();
            ctx.prepare_messages(&req, &translated, &original, stream || source != Format::Claude)
                .await
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
        let mut go_body = upstream["body"].as_str().unwrap().to_owned();
        if RELOCATES_API_KEY_SYSTEM.contains(&name) {
            let relocated = with_relocated_system(&go_body);
            assert_ne!(relocated, go_body, "{name}: Go kept no caller system text top-level");
            go_body = relocated;
        }
        assert_eq!(
            random_prompt(&normalize_random(&prepared.body)),
            random_prompt(&normalize_random(&go_body)),
            "{name}: upstream body"
        );
        // The masked CCH must still sign the body as sent (signing runs last).
        assert_eq!(
            super::signing::sign(prepared.body.clone()).unwrap(),
            prepared.body,
            "{name}: CCH"
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
            let mut prepared = prepared;
            let completion = ctx.completion(&mut prepared);
            let events: Vec<String> = stream::relay(
                raw,
                prepared.reverse.clone(),
                Box::new(move |id| completion.complete(&id, &request_id)),
                Default::default(),
                Default::default(),
            )
            .map(|e| String::from_utf8(e.unwrap().to_vec()).unwrap())
            .collect()
            .await;
            assert_eq!(events, output, "{name}: SSE events");
        } else {
            let restored = alias::restore_response(reply, &prepared.reverse).unwrap();
            assert_eq!([restored], output.as_slice(), "{name}: restored response");
            let mut prepared = prepared;
            ctx.completion(&mut prepared)
                .complete(&rawjson::string(reply, "id"), &request_id);
        }
    }
    let _ = std::fs::remove_dir_all(root);
}

/// A loopback Messages upstream that keeps the body of every request it gets and
/// answers each with a minimal completed message, streamed when the request asks for
/// a stream. Message IDs count the requests.
async fn recording_upstream() -> (String, Arc<std::sync::Mutex<Vec<String>>>) {
    use axum::response::IntoResponse;
    let sent = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    let seen = sent.clone();
    let router = axum::Router::new().fallback(move |body: String| {
        let seen = seen.clone();
        async move {
            let stream = rawjson::get(&body, "stream").bool();
            let id = format!("msg_recorded_{}", {
                let mut seen = seen.lock().unwrap();
                seen.push(body);
                seen.len()
            });
            let message = serde_json::json!({"id": id, "type": "message", "role": "assistant",
                "model": "claude-opus-5-5", "content": [], "usage": {"input_tokens": 3, "output_tokens": 0}});
            if !stream {
                return ([(http::header::CONTENT_TYPE, "application/json")], message.to_string()).into_response();
            }
            let events = [
                serde_json::json!({"type": "message_start", "message": message}),
                serde_json::json!({"type": "message_delta", "delta": {"stop_reason": "end_turn"}, "usage": {"output_tokens": 1}}),
                serde_json::json!({"type": "message_stop"}),
            ];
            let sse: String = events
                .iter()
                .map(|e| format!("event: {}\ndata: {e}\n\n", e["type"].as_str().unwrap()))
                .collect();
            ([(http::header::CONTENT_TYPE, "text/event-stream")], sse).into_response()
        }
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    (base, sent)
}

/// A native Messages request from a coding client, with the session the server
/// derives for it.
fn client_request(body: String, headers: http::HeaderMap) -> ExecRequest {
    let model = rawjson::string(&body, "model");
    let stream = rawjson::get(&body, "stream").bool();
    let body = Bytes::from(body);
    enrich(ExecRequest {
        operation: Operation::Generate,
        source_format: Format::Claude,
        response_format: Format::Claude,
        stream_payloads: false,
        requested_model: model.clone(),
        model,
        original_body: body.clone(),
        body,
        stream,
        alt: None,
        session: None,
        execution_session: None,
        derived_session: None,
        resolved_model: None,
        usage: Default::default(),
        request_path: String::new(),
        headers,
        caller: Caller {
            principal: "agent-client-key".into(),
            source: "authorization",
            provider: String::new(),
        },
    })
}

/// Sends `req` and reads the whole answer, so the turn is committed.
async fn drain(executor: &ClaudeExecutor, credential: &Credential, cfg: &Config, req: ExecRequest) {
    let response = executor.execute(credential, req, cfg).await.unwrap();
    if let ResponseBody::Stream(mut events) = response.body {
        while let Some(event) = events.next().await {
            event.unwrap();
        }
    }
}

/// The date in the currentDate reminder of the first user message.
fn reminder_date(body: &str) -> String {
    let reminder = rawjson::string(body, "messages.0.content.0.text");
    reminder.split("Today's date is ").nth(1).unwrap()[..10].to_owned()
}

/// A coding client streams one long cloaked conversation through a Claude OAuth
/// login. What Anthropic's prompt cache keys on stays byte for byte the same:
///
/// - warm: the next turn repeats the previous request's prefix;
/// - changed context: an edited caller system prompt changes the request only from
///   the turn it is moved to, after the first user message;
/// - idle: a turn 45 minutes later, past local midnight, repeats the prefix too, and
///   every breakpoint has the 1-hour TTL that outlives the gap. The currentDate
///   reminder sits in the first user message; it keeps the session's first date (Go
///   v8.0.27, 6d57ac9), and only a new conversation starts on the new date.
#[tokio::test]
async fn cloaked_conversation_prefix_holds_for_warm_changed_and_idle_turns() {
    let (base, sent) = recording_upstream().await;
    let executor = ClaudeExecutor::with_client(wreq::Client::new(), DEFAULT_BASE_URL);
    let mut credential = harness_credential();
    credential.id = "midnight.json".into();
    credential.attributes.insert("base_url".into(), base);
    credential.attributes.insert("timezone".into(), "UTC".into());
    let cfg = Config::parse("").unwrap();
    let tools = r#"[{"name":"Read","description":"Read a file","input_schema":{"type":"object"}}]"#;
    let system = r#"[{"type":"text","text":"You are a coding agent. Project rules: keep diffs small."}]"#;
    let first = r#"{"role":"user","content":[{"type":"text","text":"Port the cache fixes, then report."}]}"#;
    let edited =
        r#"[{"type":"text","text":"You are a coding agent. Project rules: keep diffs small. Run the tests."}]"#;
    let request = |system: &str, messages: &str| {
        format!(
            r#"{{"model":"claude-opus-5-5","max_tokens":64,"stream":true,"system":{system},"tools":{tools},"messages":[{messages}]}}"#
        )
    };
    let turn = |messages: &str| request(system, messages);
    let send = |body: String| client_request(body, Default::default());
    let run = |req: ExecRequest| drain(&executor, &credential, &cfg, req);
    let turn_two = format!(
        r#"{first},{{"role":"assistant","content":[{{"type":"text","text":"Done with the first half."}}]}},{{"role":"user","content":[{{"type":"text","text":"Now the second half."}}]}}"#
    );
    let turn_three = format!(
        r#"{turn_two},{{"role":"assistant","content":[{{"type":"text","text":"Second half done."}}]}},{{"role":"user","content":[{{"type":"text","text":"Report."}}]}}"#
    );
    let _pins = Pins;
    set_clock(Some("2026-10-13T23:59:30Z"));
    run(send(turn(first))).await;
    set_clock(Some("2026-10-13T23:59:40Z"));
    run(send(turn(&turn_two))).await;
    set_clock(Some("2026-10-13T23:59:50Z"));
    run(send(request(edited, &turn_two))).await;
    set_clock(Some("2026-10-14T00:45:00Z"));
    run(send(turn(&turn_three))).await;
    let fresh = r#"{"role":"user","content":[{"type":"text","text":"A new conversation."}]}"#;
    run(send(turn(fresh))).await;

    let sent = sent.lock().unwrap();
    let [cold, warm, changed, idle, fresh] = &sent[..] else {
        panic!("{} requests", sent.len());
    };
    assert_eq!(
        [cold, warm, changed, idle, fresh].map(|b| reminder_date(b)),
        ["2026-10-13", "2026-10-13", "2026-10-13", "2026-10-13", "2026-10-14"]
    );
    // The selected content the cache is keyed on, in Anthropic's order (tools, system,
    // messages), up to and including message `upto`. The billing block (system.0)
    // carries a per-request signature, as in native Claude Code; that Anthropic leaves
    // it out of the key is an upstream assumption, which bench/claude-cache-live.sh
    // confirms with real cache reads. The rolling breakpoint on the last message moves
    // on with each turn, so markers after the first two messages are left out here and
    // checked by path below.
    const ROLLING: &str = r#","cache_control":{"type":"ephemeral","ttl":"1h"}"#;
    let prefix = |body: &str, upto: usize| {
        assert!(rawjson::string(body, "system.0.text").starts_with(cloak::BILLING_PREFIX));
        let mut out = rawjson::get(body, "tools").json().to_owned();
        for block in rawjson::get(body, "system").array().iter().skip(1) {
            out.push_str(block.json());
        }
        let messages = rawjson::get(body, "messages");
        for (i, message) in messages.array().iter().enumerate().take(upto + 1) {
            out.push_str(&if i < 2 {
                message.json().to_owned()
            } else {
                message.json().replace(ROLLING, "")
            });
        }
        out
    };
    let messages = |body: &str| rawjson::get(body, "messages").array().len();
    // Warm: the whole cold request (first user message, then the caller's system turn).
    assert_eq!(messages(cold), 2);
    assert_eq!(prefix(warm, 1), prefix(cold, 1));
    // Changed context: equal through the first user message, different from the
    // relocated system turn on.
    assert_eq!(prefix(changed, 0), prefix(warm, 0));
    assert_ne!(prefix(changed, 1), prefix(warm, 1));
    // Idle: the whole warm request, past midnight.
    assert_eq!(messages(warm), 4);
    assert_eq!(prefix(idle, 3), prefix(warm, 3));
    // The breakpoints: the Claude Code identity block, the first user text, the caller
    // prompt's turn and the rolling last turn, each in the 1-hour pool (an OAuth login
    // selects it, as native Claude Code does), so none lapses after 5 minutes.
    let breakpoints = |body: &str| -> Vec<String> {
        signals::blocks(body)
            .into_iter()
            .filter(|(_, block)| rawjson::get(block, "cache_control").exists())
            .map(|(path, block)| {
                assert_eq!(
                    rawjson::get(&block, "cache_control").json(),
                    r#"{"type":"ephemeral","ttl":"1h"}"#,
                    "{path}"
                );
                path
            })
            .collect()
    };
    let fixed = ["system.1", "messages.0.content.1", "messages.1.content.0"];
    let with = |last: &'static str| [&fixed[..], &[last]].concat();
    assert_eq!(breakpoints(cold), fixed);
    assert_eq!(breakpoints(warm), with("messages.3.content.0"));
    assert_eq!(breakpoints(changed), with("messages.3.content.0"));
    assert_eq!(breakpoints(idle), with("messages.5.content.0"));
    assert_eq!(breakpoints(fresh), fixed);
}

/// A cloaked API key keeps the caller's system prompt where the next turn finds it:
/// in a system turn after the first user message, on the first turn and every later
/// one, as for an OAuth login. Go v8.0.27 keeps it top-level while that user message
/// ends the conversation and moves it on the second turn, which then misses the cache
/// from the Claude Code identity block on (docs/DIFFERENCES-FROM-GO.md).
#[tokio::test]
async fn api_key_cloak_keeps_the_caller_prompt_in_place_across_turns() {
    let (base, sent) = recording_upstream().await;
    let executor = ClaudeExecutor::with_client(wreq::Client::new(), DEFAULT_BASE_URL);
    let mut credential = Credential::from_file(
        Path::new("/fake"),
        Path::new("/fake/claude-api-key.json"),
        serde_json::json!({"type": "claude"}).as_object().unwrap().clone(),
    )
    .unwrap();
    for (key, value) in [
        ("api_key", "fake-gateway-key"),
        ("base_url", &base),
        ("cloak_mode", "always"),
    ] {
        credential.attributes.insert(key.into(), value.into());
    }
    let cfg = Config::parse("").unwrap();
    let first = r#"{"role":"user","content":"Port the cache fixes."}"#;
    let second =
        format!(r#"{first},{{"role":"assistant","content":"Half done."}},{{"role":"user","content":"Go on."}}"#);
    let third = format!(r#"{second},{{"role":"assistant","content":"Done."}},{{"role":"user","content":"Report."}}"#);
    for messages in [first, &second, &third] {
        let body = format!(
            r#"{{"model":"claude-opus-5-5","max_tokens":64,"system":"You are a coding agent.","messages":[{messages}]}}"#
        );
        drain(&executor, &credential, &cfg, client_request(body, Default::default())).await;
    }
    let sent = sent.lock().unwrap();
    let [one, two, three] = &sent[..] else {
        panic!("{} requests", sent.len());
    };
    // The system blocks, the first user message and the caller's system turn.
    let prefix = |body: &str| {
        let mut out = rawjson::get(body, "system").json().to_owned();
        for message in rawjson::get(body, "messages").array().iter().take(2) {
            out.push_str(message.json());
        }
        out
    };
    assert_eq!(prefix(two), prefix(one));
    assert_eq!(prefix(three), prefix(one));
    assert_eq!(
        rawjson::get(one, "system").array().len(),
        2,
        "billing and identity only"
    );
    assert_eq!(rawjson::get(one, "messages.1.role").str(), "system");
    assert_eq!(
        rawjson::string(one, "messages.1.content.0.text"),
        "You are a coding agent."
    );
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
        stream_payloads: false,
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
            provider: String::new(),
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
/// trip, a renamed upstream model is reported, and `responses/compact` reports only its
/// summary turn, a native non-stream Execute (v8.0.35 2bc236d2).
#[tokio::test]
async fn usage_reports_follow_go_reporter_calls() {
    use axum::response::IntoResponse;
    let router = axum::Router::new().fallback(|body: String| async move {
        let model = rawjson::get(&body, "model").str().to_owned();
        if rawjson::get(&body, "stream").bool() {
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
            stream_payloads: false,
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
                provider: String::new(),
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
    // responses/compact: executeClaudeCompaction returns before its own reporter and
    // the summary turn publishes like a native Execute.
    let usage = run(
        Format::OpenAIResponse,
        false,
        Some("responses/compact"),
        Delegation::default(),
    )
    .await;
    assert_eq!(
        usage.kinds(),
        ["request", "round_trip_started", "first_byte", "body", "publish"]
    );
}

/// Owned copies of the capture events an executor records.
#[derive(Default)]
pub(super) struct Captured(pub std::sync::Mutex<Vec<String>>);

impl cpa_core::exec::CaptureObserver for Captured {
    fn record(&self, event: cpa_core::exec::CaptureEvent<'_>) {
        use cpa_core::exec::CaptureEvent::*;
        let line = match event {
            Request(r) => {
                let names: Vec<&str> = r.headers.iter().map(|(k, _)| k.as_str()).collect();
                format!(
                    "request {} {} provider={} id={} label={} type={} value={} body={} headers={}",
                    r.method,
                    r.url.split_once("//").unwrap().1.split_once('/').unwrap().1,
                    r.provider,
                    r.auth_id,
                    r.auth_label,
                    r.auth_type,
                    r.auth_value,
                    !r.body.is_empty(),
                    names.join(","),
                )
            }
            ResponseMetadata(status, headers) => {
                let names: Vec<&str> = headers.iter().map(|(k, _)| k.as_str()).collect();
                format!("metadata {status} {}", names.join(","))
            }
            ResponseChunk(chunk) => format!("chunk {}", String::from_utf8_lossy(chunk)),
            ResponseError(error) => format!("error {error}"),
            _ => "websocket".into(),
        };
        self.0.lock().unwrap().push(line);
    }
}

/// Go's request-log sites in claude_executor_execute.go, _stream.go and _tokens.go:
/// RecordAPIRequest before each send (executor-built headers, provider and
/// AccountInfo), RecordAPIResponseMetadata with Go's canonical header names, the whole
/// body (Execute, count_tokens, error bodies) or each scanner line (ExecuteStream) as
/// chunks, and RecordAPIResponseError on transport failures.
#[tokio::test]
async fn request_logs_follow_go_capture_sites() {
    use axum::response::IntoResponse;
    let wire: Arc<std::sync::Mutex<Vec<String>>> = Arc::default();
    let seen = wire.clone();
    let router = axum::Router::new().fallback(move |uri: http::Uri, sent: http::HeaderMap, body: String| async move {
        *seen.lock().unwrap() = sent.keys().map(|k| k.as_str().to_owned()).collect();
        let mut headers = http::HeaderMap::new();
        headers.insert(http::header::CONTENT_TYPE, "application/json".parse().unwrap());
        headers.append("x-dup", "one".parse().unwrap());
        headers.append("x-dup", "two".parse().unwrap());
        if uri.path().ends_with("count_tokens") {
            return (headers, r#"{"input_tokens":4}"#.to_owned()).into_response();
        }
        if body.contains("rate me") {
            return (
                http::StatusCode::TOO_MANY_REQUESTS,
                headers,
                r#"{"type":"error","error":{"type":"rate_limit_error","message":"slow"}}"#.to_owned(),
            )
                .into_response();
        }
        if rawjson::get(&body, "stream").bool() {
            let events = "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"m\",\"usage\":{\"input_tokens\":1}}}\n\nevent: message_stop\ndata: {\"type\":\"message_stop\"}\n\n";
            return ([(http::header::CONTENT_TYPE, "text/event-stream")], events.to_owned()).into_response();
        }
        (headers, r#"{"id":"m","type":"message","role":"assistant","model":"claude-sonnet-4-6","content":[],"stop_reason":"end_turn","usage":{"input_tokens":1,"output_tokens":1}}"#.to_owned()).into_response()
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    let closed = {
        let probe = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        format!("http://{}", probe.local_addr().unwrap())
    };
    let executor = ClaudeExecutor::with_client(wreq::Client::new(), DEFAULT_BASE_URL);
    let credential = |base_url: &str| {
        let mut c = Credential::from_file(
            Path::new("/fake"),
            Path::new("/fake/claude.json"),
            serde_json::json!({"type":"claude"}).as_object().unwrap().clone(),
        )
        .unwrap();
        c.label = "fixture-label".into();
        c.attributes.insert("api_key".into(), " fake-gateway-key ".into());
        c.attributes.insert("base_url".into(), base_url.to_owned());
        c
    };
    let cfg = Config::parse("").unwrap();
    let run = |base_url: String, text: &'static str, operation: Operation, stream: bool, delegation: Delegation| {
        let captured = Arc::new(Captured::default());
        let body = Bytes::from(format!(
            r#"{{"model":"claude-sonnet-4-6","max_tokens":8,"stream":{stream},"messages":[{{"role":"user","content":"{text}"}}]}}"#
        ));
        let req = ExecRequest {
            operation,
            source_format: Format::Claude,
            response_format: Format::Claude,
            stream_payloads: false,
            requested_model: "claude-sonnet-4-6".into(),
            model: "claude-sonnet-4-6".into(),
            original_body: body.clone(),
            body,
            stream,
            alt: None,
            session: None,
            execution_session: None,
            derived_session: None,
            resolved_model: None,
            usage: cpa_core::exec::UsageSink::default()
                .with_capture(cpa_core::exec::CaptureSink::new(captured.clone())),
            request_path: String::new(),
            headers: Default::default(),
            caller: Caller {
                principal: "fake-client".into(),
                source: "x-api-key",
                provider: String::new(),
            },
        };
        let (executor, cfg) = (&executor, &cfg);
        async move {
            let credential = credential(&base_url);
            if let Ok(ExecResponse {
                body: ResponseBody::Stream(events),
                ..
            }) = executor.execute_delegated(&credential, req, cfg, delegation).await
            {
                let _: Vec<_> = events.collect().await;
            }
            std::mem::take(&mut *captured.0.lock().unwrap())
        }
    };
    let request = |path: &str, provider: &str| {
        format!(
            "request POST {path}?beta=true provider={provider} id=claude.json label=fixture-label type=api_key value=fake-gateway-key body=true headers="
        )
    };
    // The logged headers are exactly the executor's own headers on the wire: everything
    // the upstream received except what the transport adds (Host, Content-Length).
    let logged_match_wire = |event: &str| {
        let mut logged: Vec<String> = event
            .rsplit_once("headers=")
            .unwrap()
            .1
            .split(',')
            .map(GoStr::go_lower)
            .collect();
        let mut sent: Vec<String> = wire
            .lock()
            .unwrap()
            .iter()
            .filter(|k| !matches!(k.as_str(), "host" | "content-length"))
            .cloned()
            .collect();
        logged.sort();
        sent.sort();
        assert_eq!(logged, sent, "{event}");
    };
    let metadata = "metadata 200 Content-Type,X-Dup,X-Dup,Content-Length,Date";
    // Execute: the whole decoded body as one chunk.
    let events = run(base.clone(), "hi", Operation::Generate, false, Delegation::default()).await;
    assert_eq!(events.len(), 3, "{events:?}");
    assert!(events[0].starts_with(&request("v1/messages", "claude")), "{events:?}");
    logged_match_wire(&events[0]);
    assert_eq!(events[1], metadata);
    assert!(events[2].starts_with(r#"chunk {"id":"m""#), "{events:?}");
    // ExecuteStream: every scanner line, blank lines included, up to message_stop.
    let events = run(base.clone(), "hi", Operation::Generate, true, Delegation::default()).await;
    assert!(events[0].starts_with(&request("v1/messages", "claude")), "{events:?}");
    logged_match_wire(&events[0]);
    assert!(events[1].starts_with("metadata 200 Content-Type"), "{events:?}");
    assert_eq!(
        events[2..],
        [
            "chunk event: message_start",
            r#"chunk data: {"type":"message_start","message":{"id":"m","usage":{"input_tokens":1}}}"#,
            "chunk ",
            "chunk event: message_stop",
            r#"chunk data: {"type":"message_stop"}"#,
            "chunk ",
        ]
    );
    // An upstream error: its metadata, then the error body.
    let events = run(
        base.clone(),
        "rate me",
        Operation::Generate,
        false,
        Delegation::default(),
    )
    .await;
    assert_eq!(events[1], metadata.replace("200", "429"));
    assert!(events[2].starts_with(r#"chunk {"type":"error""#), "{events:?}");
    // Kimi's embedded executor counts upstream and logs provider kimi.
    let kimi = Delegation {
        count_upstream: true,
        request_log_provider: Some("kimi"),
        ..Default::default()
    };
    let events = run(base.clone(), "hi", Operation::CountTokens, false, kimi).await;
    assert_eq!(events.len(), 3, "{events:?}");
    assert!(
        events[0].starts_with(&request("v1/messages/count_tokens", "kimi")),
        "{events:?}"
    );
    logged_match_wire(&events[0]);
    assert_eq!(
        events[1..],
        [metadata.to_owned(), r#"chunk {"input_tokens":4}"#.to_owned()]
    );
    // A transport failure: the request, then the error.
    let events = run(closed, "hi", Operation::Generate, false, Delegation::default()).await;
    assert_eq!(events.len(), 2, "{events:?}");
    assert!(events[1].starts_with("error "), "{events:?}");
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
            stream_payloads: false,
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
                provider: String::new(),
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

/// The refused-version line is written only when the baseline identity really replaces
/// the caller's: an API key without cloaking forwards the caller's own User-Agent, so
/// it never logs; an OAuth account sends the baseline identity, so it does.
#[tokio::test]
async fn refused_versions_log_only_when_the_identity_is_replaced() {
    let router = axum::Router::new().fallback(|| async {
        (
            [("content-type", "application/json")],
            r#"{"id":"msg_q","type":"message","role":"assistant","model":"m","content":[],"stop_reason":"end_turn","usage":{"input_tokens":1,"output_tokens":1}}"#,
        )
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    let executor = ClaudeExecutor::with_client(wreq::Client::new(), DEFAULT_BASE_URL);
    let credential = |file: &str, meta: Value, key: Option<&str>| {
        let mut c = Credential::from_file(
            Path::new("/fake"),
            &Path::new("/fake").join(file),
            meta.as_object().unwrap().clone(),
        )
        .unwrap();
        if let Some(key) = key {
            c.attributes.insert("api_key".into(), key.into());
        }
        c.attributes.insert("base_url".into(), base.clone());
        c
    };
    let request = || {
        let body = Bytes::from_static(br#"{"model":"m","max_tokens":8,"messages":[{"role":"user","content":"hi"}]}"#);
        let mut headers = http::HeaderMap::new();
        headers.insert("user-agent", "claude-cli/2.1.220 (external, cli)".parse().unwrap());
        ExecRequest {
            operation: Operation::Generate,
            source_format: Format::Claude,
            response_format: Format::Claude,
            stream_payloads: false,
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
            headers,
            caller: Caller {
                principal: "fake-client".into(),
                source: "x-api-key",
                provider: String::new(),
            },
        }
    };
    let cfg = Config::parse("").unwrap();
    let api_key = credential(
        "refused-api-key.json",
        serde_json::json!({"type":"claude"}),
        Some("fake-gateway-key"),
    );
    executor.execute(&api_key, request(), &cfg).await.unwrap();
    assert!(
        !profile::refused_logged(&api_key.id),
        "the caller's own User-Agent went upstream"
    );
    let mut meta = Value::Object(harness_credential().metadata.clone());
    meta["access_token"] = "sk-ant-oat01-FAKE".into();
    let oauth = credential("refused-oauth.json", meta, None);
    executor.execute(&oauth, request(), &cfg).await.unwrap();
    assert!(
        profile::refused_logged(&oauth.id),
        "the baseline identity went upstream"
    );
}

/// `upstream.claude.model-level-cooling` (Go 52d5507d) in every spelling, seen through
/// the API-key view: off, a shared-window 429 cools the credential; on, the model.
#[test]
fn shared_window_429_follows_model_level_cooling() {
    for (yaml, scope) in [
        ("{}\n", FailureScope::Credential),
        (
            "upstream: {claude: {model-level-cooling: false}}\n",
            FailureScope::Credential,
        ),
        ("upstream: {claude: {model-level-cooling: true}}\n", FailureScope::Model),
        (
            "oauth: {providers: {claude: {model-level-cooling: true}}}\n",
            FailureScope::Model,
        ),
        ("claude: {model-level-cooling: true}\n", FailureScope::Model),
    ] {
        let cfg = Config::parse(yaml).unwrap();
        let settings = settings::Settings::from_config(&cfg.for_api_key());
        let mut headers = http::HeaderMap::new();
        headers.insert("anthropic-ratelimit-unified-5h-status", "rejected".parse().unwrap());
        let error = upstream_error(
            429,
            headers,
            Bytes::from_static(b"{}"),
            false,
            settings.model_level_cooling,
        );
        assert_eq!(error.scope, scope, "{yaml}");
    }
}

/// `tests/fixtures/claude_ratelimit_go.json`, recorded from v8.0.35 by
/// `tests/reference/claude_ratelimit`: the per-minute 429 cooldown (db1bab01), the
/// header-only cooldown it replaces, and the scope and retry of a classified and a fast
/// direct 429.
#[test]
fn claude_rate_limit_cooldown_and_scope_match_go() {
    let fixture: Value = serde_json::from_str(include_str!("../../tests/fixtures/claude_ratelimit_go.json")).unwrap();
    let fixed = std::time::UNIX_EPOCH + Duration::from_secs(fixture["now_unix"].as_u64().unwrap());
    let headers = |case: &Value, now: std::time::SystemTime| -> http::HeaderMap {
        let unix = now.duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();
        let expand = |raw: &str| {
            let Some((kind, rest)) = raw.strip_prefix('{').and_then(|r| r.split_once('+')) else {
                return raw.to_owned();
            };
            let offset: u64 = rest.trim_end_matches('}').parse().unwrap();
            match kind {
                "now" => (unix + offset).to_string(),
                _ => httpdate::fmt_http_date(now + Duration::from_secs(offset)),
            }
        };
        let mut map = http::HeaderMap::new();
        for pair in case["headers"].as_array().unwrap() {
            let name = http::HeaderName::from_bytes(pair[0].as_str().unwrap().as_bytes()).unwrap();
            map.append(name, expand(pair[1].as_str().unwrap()).parse().unwrap());
        }
        map
    };
    let millis = |d: Option<Duration>| d.map(|d| d.as_millis() as u64);
    let scope = |scope: FailureScope| match scope {
        FailureScope::Credential => "credential",
        FailureScope::Model => "model",
        FailureScope::Request => "request",
        other => panic!("unexpected scope {other:?}"),
    };
    for case in fixture["cases"].as_array().unwrap() {
        let name = case["name"].as_str().unwrap();
        let body = Bytes::from(case["body"].as_str().unwrap().to_owned());
        let at_fixed = headers(case, fixed);
        // DIFFERENCES-FROM-GO.md: of two byte-identical keys only the last counts, so a
        // later `"error":null` hides the per-minute error and the header-only result stays.
        let reset_ms = if name == "later null keeps the error" {
            case["header_only_ms"].as_u64()
        } else {
            case["reset_ms"].as_u64()
        };
        assert_eq!(
            millis(crate::quota::reset_for_error(&at_fixed, &body, fixed, Duration::ZERO)),
            reset_ms,
            "{name}: cooldown"
        );
        assert_eq!(
            millis(crate::quota::reset(&at_fixed, fixed, Duration::ZERO)),
            case["header_only_ms"].as_u64(),
            "{name}: header-only cooldown"
        );
        assert_eq!(
            crate::quota::shared_rejection(&at_fixed),
            case["shared"].as_bool().unwrap(),
            "{name}"
        );
        let live = headers(case, std::time::SystemTime::now());
        let classified = upstream_error(429, live.clone(), body.clone(), false, false);
        assert_eq!(scope(classified.scope), case["scope"], "{name}: scope");
        assert_eq!(
            classified.retry_after.is_some(),
            case["classify_retry"],
            "{name}: retry"
        );
        let fast = upstream_error(429, live, body, true, false);
        assert_eq!(scope(fast.scope), case["fast_scope"], "{name}: fast scope");
        assert_eq!(fast.retry_after.is_some(), case["fast_retry"], "{name}: fast retry");
        // Live cooldowns carry the 1-30 s fuzz on top of the fixed-clock base; the
        // offsets are whole seconds from a clock that has a fraction.
        for (path, retry) in [("classified", classified.retry_after), ("fast", fast.retry_after)] {
            if let (Some(retry), Some(base)) = (retry, reset_ms) {
                let ms = retry.as_millis() as u64;
                assert!(
                    ms > base.saturating_sub(2000) && ms <= base + 30_000,
                    "{name}: {path} live cooldown {ms} vs {base}"
                );
            }
        }
    }
}

/// The cases of v8.0.35's claude_executor_compaction_test.go (2bc236d2): a compaction
/// trigger or `responses/compact` becomes one non-stream Claude summary turn sealed into
/// a single compaction item; replayed CPA capsules expand, foreign ones are dropped and
/// broken ones fail with 400 before any upstream call.
#[tokio::test]
async fn responses_compaction_matches_go() {
    use axum::response::IntoResponse;
    const PROMPT: &str = "Please provide a concise and comprehensive summary";
    const SUMMARY: &str = "Sealed summary of the build.";
    let seen = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    let reply = Arc::new(std::sync::Mutex::new(String::new()));
    let router = {
        let (seen, reply) = (seen.clone(), reply.clone());
        axum::Router::new().fallback(move |body: String| {
            let (seen, reply) = (seen.clone(), reply.clone());
            async move {
                let stream = rawjson::get(&body, "stream").bool();
                seen.lock().unwrap().push(body);
                if stream {
                    let events = "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_c\",\"type\":\"message\",\"role\":\"assistant\",\"model\":\"claude-haiku-4-5-20251001\",\"content\":[],\"usage\":{\"input_tokens\":11,\"output_tokens\":0}}}\n\n\
                                  event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":7}}\n\n\
                                  event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n";
                    return ([(http::header::CONTENT_TYPE, "text/event-stream")], events).into_response();
                }
                let usage = reply.lock().unwrap().clone();
                // "empty": a 200 without any text block.
                let content = if usage == "empty" { String::new() } else { format!(r#"{{"type":"text","text":"{SUMMARY}"}}"#) };
                let usage = if usage.is_empty() || usage == "empty" { r#"{"input_tokens":11,"output_tokens":7}"#.to_owned() } else { usage };
                let body = format!(
                    r#"{{"id":"msg_compact","type":"message","role":"assistant","model":"claude-haiku-4-5-20251001","content":[{content}],"stop_reason":"end_turn","usage":{usage}}}"#
                );
                ([(http::header::CONTENT_TYPE, "application/json")], body).into_response()
            }
        })
    };
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
    credential.attributes.insert("api_key".into(), "key-compaction".into());
    credential.attributes.insert("base_url".into(), base);
    let run = |payload: String, original: Option<&str>, stream: bool, alt: Option<&str>, cfg: Config| {
        let body = Bytes::from(payload);
        let usage = Arc::new(Usage::default());
        let req = ExecRequest {
            operation: Operation::Generate,
            source_format: Format::OpenAIResponse,
            response_format: Format::OpenAIResponse,
            stream_payloads: false,
            requested_model: "claude-haiku-4-5-20251001".into(),
            model: "claude-haiku-4-5-20251001".into(),
            original_body: original.map_or_else(|| body.clone(), |o| Bytes::from(o.to_owned())),
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
                provider: String::new(),
            },
        };
        seen.lock().unwrap().clear();
        let (executor, credential, seen) = (&executor, &credential, seen.clone());
        async move {
            let result = executor.execute(credential, req, &cfg).await;
            let body = match result {
                Ok(ExecResponse {
                    body: ResponseBody::Buffered(b),
                    ..
                }) => Ok(String::from_utf8(b.to_vec()).unwrap()),
                Ok(ExecResponse {
                    body: ResponseBody::Stream(events),
                    ..
                }) => {
                    let chunks: Vec<_> = events.collect().await;
                    Ok(chunks
                        .into_iter()
                        .map(|c| String::from_utf8(c.unwrap().to_vec()).unwrap())
                        .collect())
                }
                Err(e) => Err(e),
            };
            let upstream = seen.lock().unwrap().clone();
            (body, upstream, usage.kinds())
        }
    };
    let none = || Config::parse("").unwrap();
    let input = |items: &str, extra: &str| {
        format!(r#"{{"model":"claude-haiku-4-5-20251001","stream":false{extra},"input":[{items}]}}"#)
    };
    let user = r#"{"type":"message","role":"user","content":[{"type":"input_text","text":"Check the build next."}]}"#;
    let assistant = r#"{"type":"message","role":"assistant","content":[{"type":"output_text","text":"OK, I will check the build."}]}"#;
    let tools = r#","tools":[{"type":"function","name":"check_build","description":"Check the build","parameters":{"type":"object","properties":{}}}]"#;
    let calls = r#"{"type":"function_call","call_id":"call_1","name":"check_build","arguments":"{\"target\":\"api\"}"},{"type":"function_call_output","call_id":"call_1","output":"build ok"}"#;
    let trigger = r#"{"type":"compaction_trigger"}"#;
    let capsule = |summary: &str| {
        let sealed = crate::antigravity_compaction::seal(summary, "claude-haiku-4-5-20251001").unwrap();
        format!(r#"{{"type":"compaction","encrypted_content":"{sealed}"}}"#)
    };
    // The single compaction item, its capsule and the usage of the answer.
    let compaction = |payload: &str, usage: (i64, i64, i64, i64)| {
        let item = rawjson::get(payload, "output");
        let items = item.array();
        assert_eq!(items.len(), 1, "{payload}");
        assert_eq!(items[0].get("type").str(), "compaction");
        let sealed = items[0].get("encrypted_content").str().to_owned();
        assert_eq!(crate::antigravity_compaction::unseal(&sealed).unwrap(), SUMMARY);
        let got = rawjson::get(payload, "usage");
        assert_eq!(
            (
                got.get("input_tokens").i64(),
                got.get("output_tokens").i64(),
                got.get("total_tokens").i64(),
                got.get("input_tokens_details.cached_tokens").i64()
            ),
            usage,
            "{payload}"
        );
    };
    let summary_request = |upstream: &[String]| -> String {
        assert_eq!(upstream.len(), 1, "one summary turn");
        let body = upstream[0].clone();
        assert!(body.contains(PROMPT), "{body}");
        assert!(!body.contains("compaction_trigger"), "{body}");
        assert!(!rawjson::get(&body, "stream").bool(), "{body}");
        body
    };

    // CompactionTriggerReturnsSingleCompactionItem, and the responses/compact alt.
    for alt in [None, Some("responses/compact")] {
        let items = if alt.is_some() {
            format!("{user},{assistant}")
        } else {
            format!("{user},{assistant},{trigger}")
        };
        let (body, upstream, _) = run(input(&items, ""), None, false, alt, none()).await;
        compaction(&body.unwrap(), (11, 7, 18, 0));
        summary_request(&upstream);
    }
    // CompactionTriggerStreamReturnsSingleCompactionItem: the summary turn stays
    // non-stream; the client gets the five Responses events.
    let (body, upstream, _) = run(
        input(&format!("{user},{assistant},{trigger}"), ""),
        None,
        true,
        None,
        none(),
    )
    .await;
    let body = body.unwrap();
    assert_eq!(body.matches("event: ").count(), 5, "{body}");
    let completed = body
        .split("\n\n")
        .find(|f| f.starts_with("event: response.completed"))
        .unwrap();
    compaction(
        rawjson::get(completed.split_once("data: ").unwrap().1, "response").json(),
        (11, 7, 18, 0),
    );
    summary_request(&upstream);
    // ReplayedCompactionCapsuleIsExpanded: an ordinary turn, with the summary as context.
    let continue_ = r#"{"type":"message","role":"user","content":[{"type":"input_text","text":"continue"}]}"#;
    let (body, upstream, _) = run(
        input(&format!("{},{continue_}", capsule(SUMMARY)), ""),
        None,
        false,
        None,
        none(),
    )
    .await;
    assert!(body.is_ok());
    assert!(
        upstream[0].contains(SUMMARY) && !upstream[0].contains(r#""type":"compaction""#),
        "{}",
        upstream[0]
    );
    assert!(!upstream[0].contains(PROMPT));
    // CompactionTriggerKeepsToolsForToolHistory.
    let (body, upstream, _) = run(
        input(&format!("{user},{calls},{trigger}"), tools),
        None,
        false,
        None,
        none(),
    )
    .await;
    compaction(&body.unwrap(), (11, 7, 18, 0));
    let sent = summary_request(&upstream);
    assert!(
        sent.contains("check_build")
            && sent.contains(r#""type":"tool_use""#)
            && sent.contains(r#""type":"tool_result""#),
        "{sent}"
    );
    assert_eq!(rawjson::get(&sent, "tool_choice.type").str(), "none", "{sent}");
    // CompactionTriggerFlattensToolHistoryWithoutTools.
    let (_, upstream, _) = run(
        input(&format!("{user},{calls},{trigger}"), ""),
        None,
        false,
        None,
        none(),
    )
    .await;
    let sent = summary_request(&upstream);
    assert!(
        !sent.contains(r#""type":"tool_use""#) && !sent.contains(r#""type":"tool_result""#),
        "{sent}"
    );
    assert!(
        sent.contains(r#"Tool call check_build (call_1): {\"target\":\"api\"}"#),
        "{sent}"
    );
    assert!(sent.contains("Tool result call_1: build ok"), "{sent}");
    assert!(!rawjson::get(&sent, "tool_choice").exists(), "{sent}");
    // CompactionSummaryPayloadRuleIsLast: payload rules see the finalized body.
    let ruled = Config::parse(
        "payload:\n  override:\n    - models: [{name: claude-haiku-4-5-20251001}]\n      params: {max_tokens: 1234, tool_choice: {type: auto}}\n",
    )
    .unwrap();
    let (_, upstream, _) = run(
        input(&format!("{user},{calls},{trigger}"), tools),
        None,
        false,
        None,
        ruled,
    )
    .await;
    let sent = summary_request(&upstream);
    assert_eq!(rawjson::get(&sent, "max_tokens").i64(), 1234, "{sent}");
    assert_eq!(rawjson::get(&sent, "tool_choice.type").str(), "auto", "{sent}");
    // CompactionTriggerIncludesPreviousCapsule.
    let earlier = capsule("Earlier sealed summary.");
    let (body, upstream, _) = run(
        input(&format!("{earlier},{continue_},{assistant},{trigger}"), ""),
        None,
        false,
        None,
        none(),
    )
    .await;
    compaction(&body.unwrap(), (11, 7, 18, 0));
    assert!(summary_request(&upstream).contains("Earlier sealed summary."));
    // ForeignCompactionCapsuleIsDropped.
    let foreign = r#"{"type":"compaction","encrypted_content":"gAAAAA-openai-native"}"#;
    let (body, upstream, _) = run(
        input(&format!("{foreign},{continue_},{trigger}"), ""),
        None,
        false,
        None,
        none(),
    )
    .await;
    compaction(&body.unwrap(), (11, 7, 18, 0));
    assert!(!summary_request(&upstream).contains("gAAAAA-openai-native"));
    // CompactionUsageMatchesResponsesAccounting: cache writes and reads count as input.
    *reply.lock().unwrap() =
        r#"{"input_tokens":11,"output_tokens":7,"cache_creation_input_tokens":3,"cache_read_input_tokens":100}"#.into();
    let (body, _, _) = run(input(&format!("{user},{trigger}"), ""), None, false, None, none()).await;
    compaction(&body.unwrap(), (114, 7, 121, 100));
    reply.lock().unwrap().clear();
    // InvalidCompactionCapsuleReturns400, before any upstream call.
    let broken = r#"{"type":"compaction","encrypted_content":"cpa-ag-compact-v1:not-valid-base64"}"#;
    // Go rejects it before its usage reporter exists, so no record is published, also
    // when only the original request carries the capsule.
    let plain = input(continue_, "");
    let broken = input(&format!("{broken},{continue_}"), "");
    for (body, original) in [(broken.clone(), None), (plain, Some(broken.as_str()))] {
        let (body, upstream, usage) = run(body, original, false, None, none()).await;
        let error = body.unwrap_err();
        assert_eq!(error.status, 400);
        assert!(String::from_utf8_lossy(&error.body).starts_with("invalid compaction capsule: "));
        assert!(upstream.is_empty());
        assert_eq!(usage, ["discard"]);
    }
    // A summary answer without text fails like Go's fmt.Errorf: no status and no
    // transport fault, so it cools the model on the trigger path and is neutral on the
    // compact alt, without a retry round (cpa-server classify tests); the summary turn
    // itself published its usage.
    *reply.lock().unwrap() = "empty".into();
    for (items, alt) in [
        (format!("{user},{trigger}"), None),
        (user.to_owned(), Some("responses/compact")),
    ] {
        let (body, upstream, usage) = run(input(&items, ""), None, false, alt, none()).await;
        let error = body.unwrap_err();
        assert_eq!((error.status, error.scope), (0, FailureScope::Model), "{alt:?}");
        assert_eq!(
            String::from_utf8_lossy(&error.body),
            "extract summary: no summary text found in upstream response"
        );
        summary_request(&upstream);
        assert!(usage.contains(&"publish"), "{usage:?}");
    }
    reply.lock().unwrap().clear();
}
