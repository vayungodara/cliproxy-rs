//! Differential tests against `tests/fixtures/antigravity_go.json` `executor`, produced by
//! the real Go `AntigravityExecutor` (tests/reference/antigravity/executor.go), and
//! `antigravity_v8_0_27_go.json`, the behaviour ported from Go v8.0.27 recorded the same
//! way from that tree (tests/reference/antigravity/v8_0_27.go), and
//! `antigravity_v8_0_35_go.json`, likewise from v8.0.35 (v8_0_35.go). Each
//! scenario replays the scripted upstream answer to the Rust executor and compares the
//! raw upstream request, the output, the client stream bytes, errors and the capture
//! of the attempt. Expected values come only from Go. Scenarios run in fixture order on
//! one executor, as Go's process-wide cooldown state carried over between them.

use std::sync::Arc;

use cpa_core::exec::Caller;
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

use super::*;
use crate::gemini::tests::{Captured, Captures, Mock, UsageReports, check_attempt, client_bytes};

fn fixture() -> &'static Value {
    static PARSED: std::sync::OnceLock<Value> = std::sync::OnceLock::new();
    PARSED.get_or_init(|| serde_json::from_str(include_str!("../tests/fixtures/antigravity_go.json")).unwrap())
}

fn scenarios() -> &'static [Value] {
    fixture()["executor"].as_array().unwrap()
}

fn scenario(name: &str) -> &'static Value {
    scenarios().iter().find(|s| s["name"] == name).unwrap()
}

fn credential(s: &Value, addr: &str) -> Credential {
    let replace = |v: &Value| match v {
        Value::String(text) => Value::String(text.replace("UPSTREAM", addr)),
        other => other.clone(),
    };
    let metadata: serde_json::Map<String, Value> = s["metadata"]
        .as_object()
        .unwrap()
        .iter()
        .map(|(k, v)| (k.clone(), replace(v)))
        .collect();
    let mut c = Credential::from_file(
        std::path::Path::new("/fixture"),
        std::path::Path::new("/fixture/antigravity.json"),
        metadata,
    )
    .unwrap();
    c.id = s["auth_id"].as_str().unwrap().to_owned();
    for (k, v) in s["attributes"].as_object().into_iter().flatten() {
        c.attributes
            .insert(k.clone(), v.as_str().unwrap().replace("UPSTREAM", addr));
    }
    c
}

fn request(s: &Value) -> ExecRequest {
    let op = s["op"].as_str().unwrap();
    let source = Format::parse(s["source"].as_str().unwrap()).unwrap();
    let response = s["response"].as_str().and_then(Format::parse).unwrap_or(source);
    let model = s["model"].as_str().unwrap().to_owned();
    let body = Bytes::from(s["payload"].as_str().unwrap().to_owned());
    let original = s["original"]
        .as_str()
        .map(|o| Bytes::from(o.to_owned()))
        .unwrap_or_else(|| body.clone());
    let mut headers = http::HeaderMap::new();
    for (k, v) in s["headers"].as_object().into_iter().flatten() {
        headers.insert(
            http::HeaderName::from_bytes(k.as_bytes()).unwrap(),
            v.as_str().unwrap().parse().unwrap(),
        );
    }
    ExecRequest {
        operation: if op == "count" {
            Operation::CountTokens
        } else {
            Operation::Generate
        },
        source_format: source,
        response_format: response,
        stream_payloads: false,
        requested_model: model.clone(),
        model,
        original_body: original,
        body,
        stream: op == "stream",
        alt: s["alt"].as_str().map(str::to_owned),
        session: None,
        execution_session: None,
        derived_session: s["derived_session"].as_str().map(str::to_owned),
        resolved_model: None,
        usage: Default::default(),
        request_path: String::new(),
        headers,
        caller: Caller {
            principal: "fake-client-key".into(),
            source: "authorization",
            provider: String::new(),
        },
    }
}

/// Values Go draws from the clock or a random source, reduced to their shape. The
/// default user agent's version is the process-wide cache the version tests change.
fn normalize(text: &str) -> String {
    static RULES: std::sync::OnceLock<Vec<(regex::Regex, &'static str)>> = std::sync::OnceLock::new();
    let rules = RULES.get_or_init(|| {
        [
            (r#""requestId":"agent-[0-9a-f-]{36}""#, r#""requestId":"agent-<uuid>""#),
            (
                r#""requestId":"image_gen/[0-9]{13}/[0-9a-f-]{36}/12""#,
                r#""requestId":"image_gen/<ms>/<uuid>/12""#,
            ),
            (r#""(created_at|completed_at|created)":1[0-9]{9}"#, r#""$1":<unix>"#),
            (r"(resp|cmp)_ag_compact_[0-9]+", "${1}_ag_compact_<nano>"),
            (r"cpa-ag-compact-v1:[A-Za-z0-9_-]+", "cpa-ag-compact-v1:<capsule>"),
            (r"srvtoolu_[0-9]+", "srvtoolu_<nano>"),
            // Gemini-to-Responses IDs: `resp_<UnixNano hex>_<n>`, n a process-wide counter.
            (r"resp_[0-9a-f]{16}_[0-9]+", "resp_<nano>_<n>"),
            // Interactions: `interaction_<UnixNano>` and RFC 3339 wall-clock stamps.
            (r"interaction_1[0-9]{18}", "interaction_<nano>"),
            (
                r#""(created|updated)":"[0-9]{4}-[0-9]{2}-[0-9]{2}T[^"]*""#,
                r#""$1":"<time>""#,
            ),
            // Responses function-call IDs: `call_<UnixNano>_<n>`, n a process-wide counter
            // other tests in this binary advance too.
            (r"call_1[0-9]{18}_[0-9]+", "call_<nano>_<n>"),
            // OpenAI chat tool-call IDs: `<name>-<UnixNano>-<n>`.
            (r#""id": ?"([^"]+)-1[0-9]{18}-([0-9]+)""#, r#""id":"$1-<nano>-$2""#),
            (
                r"antigravity/hub/[0-9.]+ darwin/arm64",
                "antigravity/hub/<v> darwin/arm64",
            ),
            (
                r"short cooldown, [0-9.]+(ms|s) remaining",
                "short cooldown, <d> remaining",
            ),
        ]
        .into_iter()
        .map(|(pattern, with)| (regex::Regex::new(pattern).unwrap(), with))
        .collect()
    });
    let mut out = text.to_owned();
    for (pattern, with) in rules {
        out = pattern.replace_all(&out, *with).into_owned();
    }
    out
}

/// `left == right`, failing with the first differing offset and its surroundings.
fn assert_same(name: &str, what: &str, left: &str, right: &str) {
    if left == right {
        return;
    }
    let at = left.bytes().zip(right.bytes()).take_while(|(a, b)| a == b).count();
    let window = |s: &str| {
        let start = s.floor_char_boundary(at.saturating_sub(120));
        let end = s.ceil_char_boundary((at + 200).min(s.len()));
        s[start..end].to_owned()
    };
    panic!(
        "{name}: {what} differs at byte {at}\n rust: {}\n   go: {}",
        window(left),
        window(right)
    );
}

/// Every compaction capsule Go or Rust returned opens to the summary the scenario's
/// upstream produced.
fn check_capsules(name: &str, text: &str, summary: &str) {
    let capsules = regex::Regex::new(r"cpa-ag-compact-v1:[A-Za-z0-9_-]+").unwrap();
    for capsule in capsules.find_iter(text) {
        assert_eq!(
            compaction::unseal(capsule.as_str()).as_deref(),
            Ok(summary),
            "{name}: capsule"
        );
    }
}

#[tokio::test]
async fn go_reference_scenarios() {
    cpa_core::registry::install_overlay(Some(Arc::new(WebSearchModels)));
    let ran = replay_go_scenarios(scenarios()).await;
    assert!(ran >= 49, "fixture lost scenarios: {ran}");
}

/// The scenarios recorded from Go v8.0.27 for the behaviour ported from it.
#[tokio::test]
async fn go_v8_0_27_scenarios() {
    let fixture: Value = serde_json::from_str(include_str!("../tests/fixtures/antigravity_v8_0_27_go.json")).unwrap();
    assert_eq!(fixture["go"], "v8.0.27");
    let scenarios = fixture["executor"].as_array().unwrap();
    assert_eq!(replay_go_scenarios(scenarios).await, scenarios.len());
}

/// The scenarios recorded from Go v8.0.35 for the behaviour ported from it.
#[tokio::test]
async fn go_v8_0_35_scenarios() {
    let raw = include_str!("../tests/fixtures/antigravity_v8_0_35_go.json");
    // normalize masks the hub version; Go's never-fetched default is its fallback (8fb633c9).
    let hub = regex::Regex::new(r"antigravity/hub/([0-9.]+) darwin/arm64").unwrap();
    let versions: std::collections::BTreeSet<&str> =
        hub.captures_iter(raw).map(|c| c.get(1).unwrap().as_str()).collect();
    assert_eq!(
        versions.into_iter().collect::<Vec<_>>(),
        [crate::antigravity_version::FALLBACK_VERSION]
    );
    let fixture: Value = serde_json::from_str(raw).unwrap();
    assert_eq!(fixture["go"], "v8.0.35");
    let scenarios = fixture["executor"].as_array().unwrap();
    assert_eq!(replay_go_scenarios(scenarios).await, scenarios.len());
}

/// Go v8.0.27 (4e4dede4): a payload split over stream lines is joined before its served
/// model is observed, so the usage record names the model Go's record names; no part of
/// it is read as a usage line.
#[tokio::test]
async fn a_split_payload_reports_its_served_model() {
    #[derive(Default)]
    struct Seen {
        lines: std::sync::Mutex<Vec<Vec<u8>>>,
        models: std::sync::Mutex<Vec<Vec<u8>>>,
    }
    impl cpa_core::exec::UsageObserver for Seen {
        fn response_body(&self, _: Format, _: &[u8]) {}
        fn response_line(&self, _: Format, line: &[u8]) {
            self.lines.lock().unwrap().push(line.to_vec());
        }
        fn request(&self, _: Format, _: &[u8]) {}
        fn response_model_line(&self, line: &[u8]) {
            self.models.lock().unwrap().push(line.to_vec());
        }
    }
    let fixture: Value = serde_json::from_str(include_str!("../tests/fixtures/antigravity_v8_0_27_go.json")).unwrap();
    let s = fixture["executor"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["name"] == "stream_split_payload_gemini")
        .unwrap();
    let mock = Mock::start(&s["upstream"]).await;
    let mut req = request(s);
    let seen = Arc::new(Seen::default());
    req.usage = cpa_core::exec::UsageSink::new(seen.clone());
    let response = AntigravityExecutor::default()
        .execute(&credential(s, mock.addr()), req, &Config::default())
        .await
        .unwrap();
    let ResponseBody::Stream(mut stream) = response.body else {
        panic!("a stream")
    };
    while let Some(item) = stream.next().await {
        item.unwrap();
    }
    let models = seen.models.lock().unwrap().clone();
    assert_eq!(models.len(), 1, "one joined payload");
    assert_eq!(
        gj::get(&models[0], "response.modelVersion").str(),
        s["usage"]["response_model"].as_str().unwrap()
    );
    assert!(
        seen.lines
            .lock()
            .unwrap()
            .iter()
            .all(|l| !l.windows(7).any(|w| w == b"joined\"")),
        "no part of the joined payload is a usage line"
    );
}

/// Replays `scenarios` in order on one executor; returns how many ran.
async fn replay_go_scenarios(scenarios: &[Value]) -> usize {
    let executor = AntigravityExecutor::default();
    let mut ran = 0;
    for s in scenarios {
        let name = s["name"].as_str().unwrap();
        if s.get("scripted").is_some_and(|v| !v.is_null()) {
            continue;
        }
        let mock = Mock::start(&s["upstream"]).await;
        let addr = mock.addr().to_owned();
        let cfg = Config::parse(s["config"].as_str().unwrap_or_default()).unwrap();
        let mut cred = credential(s, &addr);
        if s["known_credits"] == true {
            credits::set_hint(
                &cred.id,
                credits::Hint {
                    known: true,
                    available: true,
                    ..credits::Hint::default()
                },
            );
        }
        if s["credits"] == true {
            cred = credits::request_on(&cred);
        }
        let mut req = request(s);
        let reports = Arc::new(UsageReports::default());
        let captures = Arc::new(Captures::default());
        req.usage = reports.sink().with_capture(captures.sink());
        let (client, alt) = (req.response_format, req.alt.as_deref().is_some_and(|a| !a.is_empty()));
        let (mut output, mut streamed, mut error) = (None, Vec::new(), None);
        match executor.execute(&cred, req, &cfg).await {
            Err(e) => error = Some((e.status, String::from_utf8_lossy(&e.body).into_owned(), e.retry_after)),
            Ok(response) => match response.body {
                ResponseBody::Buffered(bytes) => output = Some(String::from_utf8(bytes.to_vec()).unwrap()),
                ResponseBody::Stream(mut stream) => loop {
                    // `cancel_when_idle`: the client goes away once nothing arrives.
                    let next = if s["cancel_when_idle"] == true {
                        match tokio::time::timeout(std::time::Duration::from_millis(200), stream.next()).await {
                            Ok(next) => next,
                            Err(_) => break,
                        }
                    } else {
                        stream.next().await
                    };
                    match next {
                        Some(Ok(bytes)) => streamed.extend_from_slice(&bytes),
                        Some(Err(e)) => {
                            error = Some((e.status, String::from_utf8_lossy(&e.body).into_owned(), e.retry_after))
                        }
                        None => break,
                    }
                },
            },
        }
        let want_error = s["error"].as_object();
        // Go's status 0 is an error without a status code (a scanner error); Rust
        // errors always carry one, so only the message is compared.
        let go_status = |e: &serde_json::Map<String, Value>| e["status"].as_u64().unwrap() as u16;
        assert_eq!(
            error.as_ref().map(|(status, message, _)| {
                let status = if want_error.is_some_and(|e| go_status(e) == 0) {
                    0
                } else {
                    *status
                };
                (status, normalize(message))
            }),
            want_error.map(|e| (go_status(e), normalize(e["message"].as_str().unwrap()))),
            "{name}: error"
        );
        if let Some(want) = want_error.and_then(|e| e.get("retry_after_ms")).and_then(Value::as_u64) {
            let got = error.as_ref().and_then(|e| e.2).map(|d| d.as_millis() as u64);
            if error.as_ref().is_some_and(|e| e.1.contains("short cooldown")) {
                // The cooldown left when the second request arrived: within a second.
                assert!(got.is_some_and(|g| g <= 30_000 && g + 1_000 >= want), "{name}: {got:?}");
            } else {
                assert_eq!(got, Some(want), "{name}: retry after");
            }
        }
        assert_same(
            name,
            "output",
            &normalize(output.as_deref().unwrap_or("<none>")),
            &normalize(s["output"].as_str().unwrap_or("<none>")),
        );
        let want_stream = client_bytes(client, alt, s["chunks"].as_array().map_or(&[][..], Vec::as_slice));
        assert_same(
            name,
            "stream",
            &normalize(&String::from_utf8_lossy(&streamed)),
            &normalize(&String::from_utf8_lossy(&want_stream)),
        );
        let summary =
            compaction::summary_text(s["upstream"]["body"].as_str().unwrap_or_default().as_bytes()).unwrap_or_default();
        let (got_stream, go_stream) = (
            String::from_utf8_lossy(&streamed),
            String::from_utf8_lossy(&want_stream),
        );
        for text in [
            output.as_deref(),
            s["output"].as_str(),
            Some(&got_stream),
            Some(&go_stream),
        ] {
            check_capsules(name, text.unwrap_or_default(), &summary);
        }
        let wire = mock.request();
        assert_same(
            name,
            "upstream request",
            &normalize(wire.as_deref().unwrap_or("<none>")),
            &normalize(s["request"].as_str().unwrap_or("<none>")),
        );
        reports.check(s);
        if let Some(key) = s["replay_key"].as_str() {
            let got: Vec<String> = cpa_common::cache::replay_get(s["model"].as_str().unwrap(), key)
                .unwrap_or_default()
                .into_iter()
                .map(|item| String::from_utf8(item).unwrap())
                .collect();
            let want: Vec<&str> = s["replay"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .collect();
            assert_eq!(got, want, "{name}: replay ledger");
        }
        if let Some(want) = s.get("credits_hint").filter(|h| !h.is_null()) {
            let got = credits::hint(&cred.id).unwrap_or_default();
            assert_eq!(
                (got.known, got.available, got.credit_amount, got.min_credit_amount),
                (
                    want["known"].as_bool().unwrap(),
                    want["available"].as_bool().unwrap(),
                    want["credit_amount"].as_f64().unwrap(),
                    want["min_credit_amount"].as_f64().unwrap()
                ),
                "{name}: credits hint"
            );
        }
        let mut events: std::collections::VecDeque<Captured> = captures.events().into();
        if let Some(wire) = &wire {
            let lines = wire.starts_with("POST /v1internal:streamGenerateContent");
            let email = s["metadata"]["email"].as_str().unwrap_or_default();
            let normalized_wire = normalize(wire);
            // The capture holds the request ID the Rust executor drew; compare its shape.
            let mut normalized_events: std::collections::VecDeque<Captured> = events
                .drain(..)
                .map(|event| match event {
                    Captured::Request {
                        url,
                        headers,
                        body,
                        provider,
                        auth_id,
                        auth_type,
                        auth_value,
                    } => Captured::Request {
                        url,
                        headers: headers.into_iter().map(|(n, v)| (n, normalize(&v))).collect(),
                        body: normalize(&String::from_utf8_lossy(&body)).into_bytes(),
                        provider,
                        auth_id,
                        auth_type,
                        auth_value,
                    },
                    other => other,
                })
                .collect();
            check_attempt(
                name,
                &mut normalized_events,
                &normalized_wire,
                Some(&s["upstream"]),
                lines,
                PROVIDER,
                Some(("oauth", email)),
            );
            // An error inside a 2xx stream that was read whole (a backend error object):
            // Go's RecordAPIResponseError of it. An apply_patch failure is not captured.
            let streamed_ok = s["upstream"]["status"]
                .as_u64()
                .is_some_and(|s| (200..300).contains(&s))
                && s["upstream"]["missing"].as_u64().unwrap_or(0) == 0;
            let backend =
                |e: &&serde_json::Map<String, Value>| e["message"] != cpa_translate::APPLY_PATCH_UPSTREAM_ERROR;
            if let Some(want) = want_error.filter(|_| streamed_ok).filter(backend) {
                match normalized_events.pop_front() {
                    Some(Captured::Error(text)) => {
                        assert_eq!(text, want["message"].as_str().unwrap(), "{name}: capture")
                    }
                    other => panic!("{name}: expected the stream error capture, got {other:?}"),
                }
            }
            events = normalized_events;
        }
        crate::gemini::tests::check_refused_capture(name, s, &mut events);
        assert!(events.is_empty(), "{name}: extra captures {events:?}");
        ran += 1;
    }
    ran
}

/// A TLS server for the Vertex Search redirect host answering HEAD requests in order,
/// recording each raw request head.
async fn redirect_host(
    replies: Vec<(u16, String)>,
) -> (Vec<u8>, std::net::SocketAddr, Arc<std::sync::Mutex<Vec<String>>>) {
    let hosts = vec!["vertexaisearch.cloud.google.com".to_owned()];
    let (ca, acceptor) = crate::test_tls::acceptor(&hosts, b"\x08http/1.1");
    let acceptor = Arc::new(acceptor);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let log = Arc::new(std::sync::Mutex::new(Vec::new()));
    let replies = Arc::new(std::sync::Mutex::new(std::collections::VecDeque::from(replies)));
    let (seen, queue) = (log.clone(), replies.clone());
    tokio::spawn(async move {
        while let Ok((socket, _)) = listener.accept().await {
            let (acceptor, seen, queue) = (acceptor.clone(), seen.clone(), queue.clone());
            tokio::spawn(async move {
                let ssl = btls::ssl::Ssl::new(acceptor.context()).unwrap();
                let mut stream = tokio_btls::SslStream::new(ssl, socket).unwrap();
                if std::pin::Pin::new(&mut stream).accept().await.is_err() {
                    return;
                }
                let mut reader = BufReader::new(stream);
                loop {
                    let mut head = String::new();
                    loop {
                        let mut line = String::new();
                        if reader.read_line(&mut line).await.unwrap_or(0) == 0 {
                            return;
                        }
                        head.push_str(&line);
                        if line == "\r\n" {
                            break;
                        }
                    }
                    seen.lock().unwrap().push(head);
                    let (status, location) = queue.lock().unwrap().pop_front().unwrap_or((599, String::new()));
                    let location = if location.is_empty() {
                        String::new()
                    } else {
                        format!("Location: {location}\r\n")
                    };
                    let reply = format!("HTTP/1.1 {status} X\r\n{location}Content-Length: 0\r\n\r\n");
                    if reader.get_mut().write_all(reply.as_bytes()).await.is_err() {
                        return;
                    }
                }
            });
        }
    });
    (ca, addr, log)
}

/// The installed registry answering only Antigravity's available models, which carry
/// native web search, and `gemini-3.1-pro-low`; every other lookup falls through to the
/// pinned catalog as with no registry installed.
struct WebSearchModels;

impl cpa_core::registry::Overlay for WebSearchModels {
    /// `go_stream_interactions` was recorded with 6fecc6e's catalog, which listed
    /// `gemini-3.1-pro-low` for Antigravity; Go v8.0.35 (f62d4d28) drops it. This is its
    /// definition from that catalog.
    fn lookup(&self, id: &str, provider: Option<&str>) -> Option<cpa_core::registry::ModelInfo> {
        if id != "gemini-3.1-pro-low" || provider != Some("antigravity") {
            return None;
        }
        let raw = r#"{"id":"gemini-3.1-pro-low","object":"model","owned_by":"antigravity","type":"antigravity","supports_web_search":true,"display_name":"Gemini 3.1 Pro (Low)","name":"gemini-3.1-pro-low","description":"Gemini 3.1 Pro (Low)","context_length":1048576,"max_completion_tokens":65535,"thinking":{"min":1,"max":65535,"dynamic_allowed":true,"levels":["low","medium","high"]},"supportedInputModalities":["text","image","audio","video"],"supportedOutputModalities":["text"],"native_capabilities":{"web_search":true}}"#;
        cpa_core::registry::ModelInfo::from_raw(serde_json::from_str(raw).unwrap()).ok()
    }

    fn for_credential(&self, _: &str, _: &str) -> Option<cpa_core::registry::ModelInfo> {
        None
    }

    fn available_by_provider(&self, provider: &str) -> Vec<cpa_core::registry::ModelInfo> {
        if provider != "antigravity" {
            return Vec::new();
        }
        let ids = scenario("claude_web_search_grounding")["web_search_models"]
            .as_array()
            .unwrap();
        ids.iter()
            .map(|id| {
                let mut raw = serde_json::Map::new();
                raw.insert("id".into(), id.clone());
                raw.insert("supports_web_search".into(), Value::Bool(true));
                cpa_core::registry::ModelInfo::from_raw(raw).unwrap()
            })
            .collect()
    }
}

/// Go's grounding resolution (claude_web_search_grounding): the typed web-search tool
/// becomes a Google Search request, and each distinct Vertex Search redirect in the
/// answer is resolved once with a HEAD request that does not follow the redirect, while
/// direct URIs stay (helps TestResolveAntigravityGroundingURLsResolvesVertexRedirects).
#[tokio::test]
async fn web_search_grounding_matches_go() {
    let s = scenario("claude_web_search_grounding");
    let scripted = s["scripted"].as_array().unwrap();
    let go_requests = s["requests"].as_array().unwrap();
    cpa_core::registry::install_overlay(Some(Arc::new(WebSearchModels)));
    let generate = serde_json::json!({
        "status": scripted[0]["status"],
        "headers": [["Content-Type", "application/json"]],
        "body": scripted[0]["body"],
    });
    let mock = Mock::start(&generate).await;
    let heads: Vec<(u16, String)> = scripted[1..]
        .iter()
        .map(|r| {
            (
                r["status"].as_u64().unwrap() as u16,
                r["location"].as_str().unwrap_or_default().to_owned(),
            )
        })
        .collect();
    let (ca, addr, seen) = redirect_host(heads).await;
    let executor = AntigravityExecutor::with_hooks(Hooks {
        trust: Some(wreq::tls::trust::CertStore::from_pem_certs([ca.as_slice()]).unwrap()),
        resolve: vec![("vertexaisearch.cloud.google.com".to_owned(), addr)],
    });
    let cfg = Config::parse("").unwrap();
    let cred = credential(s, mock.addr());
    let response = executor.execute(&cred, request(s), &cfg).await.unwrap();
    let ResponseBody::Buffered(out) = response.body else {
        panic!("buffered answer")
    };
    assert_eq!(
        normalize(&String::from_utf8_lossy(&out)),
        normalize(s["output"].as_str().unwrap()),
        "output"
    );
    // The generate request: Go's header map plus what its transport adds.
    let wire = mock.request().unwrap();
    let (head, body) = wire.split_once("\r\n\r\n").unwrap();
    assert_eq!(body, go_requests[0]["body"].as_str().unwrap(), "generate body");
    assert!(
        head.starts_with("POST /v1internal:generateContent HTTP/1.1\r\n"),
        "{head}"
    );
    for header in go_requests[0]["headers"].as_array().unwrap() {
        let line = format!("{}: {}", header[0].as_str().unwrap(), header[1].as_str().unwrap());
        assert!(normalize(head).contains(&normalize(&line)), "{line} in {head}");
    }
    // One HEAD per distinct redirect URI, in answer order, with only Go's default
    // transport headers (an empty header map, no Accept-Encoding for HEAD).
    let seen = seen.lock().unwrap().clone();
    let heads: Vec<(String, String)> = go_requests[1..]
        .iter()
        .map(|r| {
            let url = url::Url::parse(r["url"].as_str().unwrap()).unwrap();
            (r["method"].as_str().unwrap().to_owned(), url.path().to_owned())
        })
        .collect();
    assert_eq!(seen.len(), heads.len(), "{seen:?}");
    for (raw, (method, path)) in seen.iter().zip(&heads) {
        assert!(raw.starts_with(&format!("{method} {path} HTTP/1.1\r\n")), "{raw}");
        let names: Vec<String> = raw
            .lines()
            .skip(1)
            .filter_map(|l| l.split_once(':'))
            .map(|(n, _)| n.to_ascii_lowercase())
            .collect();
        assert_eq!(names, ["host", "user-agent"], "{raw}");
        assert!(raw.contains("User-Agent: Go-http-client/1.1\r\n"), "{raw}");
    }
}

/// Go `maybeRefreshAntigravityCreditsHint` / `updateAntigravityCreditsBalanceForTask`
/// (TestEnsureAccessToken_WarmTokenLoadsCreditsHint,
/// TestUpdateAntigravityCreditsBalance_LoadCodeAssistUserAgent): with credits on, a
/// request on a credential whose credits are unknown posts `loadCodeAssist` to its base
/// URL with Go's headers, and the Google One AI credit becomes the hint.
#[tokio::test]
async fn credits_lookup_loads_the_hint() {
    use axum::extract::Request;
    let seen: Arc<std::sync::Mutex<Vec<(String, http::HeaderMap, String)>>> = Arc::default();
    let log = seen.clone();
    let app = axum::Router::new().fallback(move |req: Request| {
        let log = log.clone();
        async move {
            let path = req.uri().path().to_owned();
            let headers = req.headers().clone();
            let body = axum::body::to_bytes(req.into_body(), usize::MAX).await.unwrap();
            log.lock()
                .unwrap()
                .push((path.clone(), headers, String::from_utf8_lossy(&body).into_owned()));
            if path.ends_with(":loadCodeAssist") {
                r#"{"paidTier":{"id":"g1-pro-tier","availableCredits":[{"creditType":"GOOGLE_ONE_AI","creditAmount":"25000","minimumCreditAmountForUsage":"50"}]}}"#.to_owned()
            } else {
                r#"{"response":{"candidates":[{"content":{"role":"model","parts":[{"text":"ok"}]},"finishReason":"STOP"}]}}"#.to_owned()
            }
        }
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let mut s = scenario("gemini_execute_basic").clone();
    s["auth_id"] = Value::String("ag-credits-lookup".into());
    s["attributes"] = serde_json::json!({ "base_url": base, "user_agent": "antigravity/1.23.2 windows/amd64" });
    let cfg = Config::parse("quota-exceeded:\n  antigravity-credits: true\n").unwrap();
    let cred = credential(&s, "");
    assert!(credits::hint(&cred.id).is_none());
    let executor = AntigravityExecutor::default();
    executor.execute(&cred, request(&s), &cfg).await.unwrap();
    let mut hint = None;
    for _ in 0..200 {
        hint = credits::hint(&cred.id);
        if hint.is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let hint = hint.expect("credits hint");
    assert_eq!(
        (
            hint.known,
            hint.available,
            hint.credit_amount,
            hint.min_credit_amount,
            hint.paid_tier_id.as_str()
        ),
        (true, true, 25000.0, 50.0, "g1-pro-tier")
    );
    let requests = seen.lock().unwrap().clone();
    let lookup = requests
        .iter()
        .find(|(path, ..)| path == "/v1internal:loadCodeAssist")
        .expect("lookup");
    assert_eq!(lookup.2, r#"{"metadata":{"ideType":"ANTIGRAVITY"}}"#);
    let header = |name: &str| {
        lookup
            .1
            .get(name)
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .to_owned()
    };
    assert_eq!(header("authorization"), "Bearer ya29.fake-ag");
    assert_eq!(header("accept"), "*/*");
    assert_eq!(header("content-type"), "application/json");
    assert_eq!(header("user-agent"), "antigravity/1.23.2 windows/amd64");
    // A known hint: no second lookup.
    executor.execute(&cred, request(&s), &cfg).await.unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;
    let requests = seen.lock().unwrap().clone();
    let count = |path: &str| requests.iter().filter(|(p, ..)| p == path).count();
    assert_eq!(
        (
            count("/v1internal:loadCodeAssist"),
            count("/v1internal:generateContent")
        ),
        (1, 2)
    );
}

/// Go f51624aa translates before `ensureAccessToken`: a request the translator refuses
/// (a Claude user turn holding only a URL image) gets its 400 with no credits lookup
/// and no other upstream call, on the generate, stream and token-count paths.
#[tokio::test]
async fn a_refused_request_makes_no_credits_lookup() {
    use axum::extract::Request;
    let paths: Arc<std::sync::Mutex<Vec<String>>> = Arc::default();
    let log = paths.clone();
    let app = axum::Router::new().fallback(move |req: Request| {
        let log = log.clone();
        async move {
            log.lock().unwrap().push(req.uri().path().to_owned());
            "{}"
        }
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let cfg = Config::parse("quota-exceeded:\n  antigravity-credits: true\n").unwrap();
    let executor = AntigravityExecutor::default();
    for op in ["execute", "stream", "count"] {
        let mut s = scenario("gemini_execute_basic").clone();
        s["auth_id"] = Value::String(format!("ag-refused-{op}"));
        s["attributes"] = serde_json::json!({ "base_url": base });
        s["op"] = Value::String(op.into());
        s["source"] = Value::String("claude".into());
        s["payload"] = Value::String(
            r#"{"model":"gemini-3-flash","max_tokens":8,"messages":[{"role":"user","content":[{"type":"image","source":{"type":"url","url":"https://example.test/a.png"}}]}]}"#.into(),
        );
        let cred = credential(&s, "");
        let err = match executor.execute(&cred, request(&s), &cfg).await {
            Ok(_) => panic!("{op}: the refused request succeeded"),
            Err(e) => e,
        };
        assert_eq!(err.status, 400, "{op}: {:?}", err.body);
        assert!(!credits::refreshing(&cred.id), "{op}: a credits lookup was queued");
    }
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(paths.lock().unwrap().clone(), Vec::<String>::new());
}

/// A credits lookup whose upstream accepts the connection and never answers ends at its
/// budget, so the credential's later lookups are not blocked for good.
#[tokio::test]
async fn a_stalled_credits_lookup_ends_at_its_budget() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let (connected, reached) = tokio::sync::oneshot::channel::<()>();
    let held = tokio::spawn(async move {
        let mut connected = Some(connected);
        let mut open = Vec::new();
        while let Ok((socket, _)) = listener.accept().await {
            open.push(socket);
            if let Some(connected) = connected.take() {
                let _ = connected.send(());
            }
        }
    });
    let mut s = scenario("gemini_execute_basic").clone();
    s["auth_id"] = Value::String("ag-credits-stalled".into());
    s["attributes"] = serde_json::json!({ "base_url": base });
    let cred = credential(&s, "");
    let budget = Duration::from_millis(300);
    AntigravityExecutor::default().queue_credits_refresh(&cred, &Config::default(), "ya29.fake-ag", budget);
    assert!(credits::refreshing(&cred.id));
    tokio::time::timeout(Duration::from_secs(5), reached)
        .await
        .expect("the lookup reached the upstream")
        .unwrap();
    let mut ended = false;
    for _ in 0..100 {
        tokio::time::sleep(Duration::from_millis(20)).await;
        if !credits::refreshing(&cred.id) {
            ended = true;
            break;
        }
    }
    assert!(ended, "the stalled lookup ended");
    held.abort();
}

/// Go TestAntigravityPoolConfigIdleTimeoutCustomAndCap.
#[test]
fn pool_settings_follow_go() {
    let pooled = |secs: u64, max: usize| PoolSettings::Pooled {
        idle_timeout: Duration::from_secs(secs),
        max_idle_per_host: max,
    };
    let cases = [
        ("", PoolSettings::Short),
        ("antigravity: {connection-pool: {enabled: false}}", PoolSettings::Short),
        ("antigravity: {connection-pool: {enabled: true}}", pooled(30, 2)),
        (
            "antigravity: {connection-pool: {enabled: true, idle-conn-timeout: 15s}}",
            pooled(15, 2),
        ),
        (
            "antigravity: {connection-pool: {enabled: true, idle-conn-timeout: 45s}}",
            pooled(45, 2),
        ),
        (
            "antigravity: {connection-pool: {enabled: true, idle-conn-timeout: 10m}}",
            pooled(210, 2),
        ),
        (
            "antigravity: {connection-pool: {enabled: true, idle-conn-timeout: 500s}}",
            pooled(210, 2),
        ),
        (
            "antigravity: {connection-pool: {enabled: true, idle-conn-timeout: 0s}}",
            PoolSettings::Short,
        ),
        (
            "antigravity: {connection-pool: {enabled: true, idle-conn-timeout: invalid-timeout}}",
            pooled(30, 2),
        ),
        (
            "antigravity: {connection-pool: {enabled: true, max-idle-conns-per-host: -5}}",
            PoolSettings::Short,
        ),
        (
            "antigravity: {connection-pool: {enabled: true, max-idle-conns-per-host: 1000000}}",
            pooled(30, 100),
        ),
        (
            "antigravity: {connection-pool: {enabled: true, max-idle-conns-per-host: 0}}",
            pooled(30, 2),
        ),
        (
            "antigravity: {connection-pool: {enabled: true, max-idle-conns-per-host: 7}}",
            pooled(30, 7),
        ),
        (
            "oauth: {providers: {antigravity: {connection-pool: {enabled: true, idle-conn-timeout: 1m30s}}}}",
            pooled(90, 2),
        ),
    ];
    for (yaml, want) in cases {
        assert_eq!(pool_settings(&Config::parse(yaml).unwrap()), want, "{yaml}");
    }
}

/// A keep-alive HTTP/1.1 server answering `replies` in order (200 `{}` once they run
/// out), counting the connections it accepted and recording each request head.
async fn keep_alive_upstream(
    replies: Vec<(u16, &'static str)>,
) -> (
    String,
    Arc<std::sync::atomic::AtomicUsize>,
    Arc<std::sync::Mutex<Vec<String>>>,
) {
    use tokio::io::AsyncReadExt;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let connections = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let heads = Arc::new(std::sync::Mutex::new(Vec::new()));
    let replies = Arc::new(std::sync::Mutex::new(std::collections::VecDeque::from(replies)));
    let (count, log) = (connections.clone(), heads.clone());
    tokio::spawn(async move {
        while let Ok((socket, _)) = listener.accept().await {
            count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let (log, replies) = (log.clone(), replies.clone());
            tokio::spawn(async move {
                let (read, mut write) = socket.into_split();
                let mut reader = BufReader::new(read);
                loop {
                    let (mut head, mut length) = (String::new(), 0usize);
                    loop {
                        let mut line = String::new();
                        if reader.read_line(&mut line).await.unwrap_or(0) == 0 {
                            return;
                        }
                        if let Some(v) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                            length = v.trim().parse().unwrap_or(0);
                        }
                        head.push_str(&line);
                        if line == "\r\n" {
                            break;
                        }
                    }
                    let mut body = vec![0; length];
                    reader.read_exact(&mut body).await.unwrap();
                    log.lock().unwrap().push(head);
                    let (status, body) = replies.lock().unwrap().pop_front().unwrap_or((
                        200,
                        r#"{"response":{"candidates":[{"content":{"role":"model","parts":[{"text":"ok"}]},"finishReason":"STOP"}]}}"#,
                    ));
                    let reply = format!(
                        "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
                        body.len()
                    );
                    if write.write_all(reply.as_bytes()).await.is_err() {
                        return;
                    }
                }
            });
        }
    });
    (base, connections, heads)
}

fn pooled_scenario(base: &str, id: &str) -> Value {
    let mut s = scenario("gemini_execute_basic").clone();
    s["auth_id"] = Value::String(id.into());
    s["attributes"] = serde_json::json!({ "base_url": base });
    s
}

/// Go TestAntigravityShortModeNeverAdvertisesConnectionCloseAndDisconnects: by default
/// every request opens its own connection, and none says `Connection: close`.
#[tokio::test]
async fn short_mode_never_reuses_or_announces_close() {
    let (base, connections, heads) = keep_alive_upstream(Vec::new()).await;
    let executor = AntigravityExecutor::default();
    let cfg = Config::parse("").unwrap();
    let s = pooled_scenario(&base, "ag-short");
    for _ in 0..3 {
        executor.execute(&credential(&s, ""), request(&s), &cfg).await.unwrap();
    }
    assert_eq!(connections.load(std::sync::atomic::Ordering::SeqCst), 3);
    for head in heads.lock().unwrap().iter() {
        assert!(!head.to_ascii_lowercase().contains("\r\nconnection:"), "{head}");
    }
}

/// Go TestNewAntigravityHTTPClientScopesPoolsByAuthIdentity,
/// TestAntigravityConcurrentRequestsReusePooledConnections and
/// TestCloseAntigravityAuthIdleTransportsEvictsIdlePool: with pooling on, a credential
/// reuses its connection, another credential never shares it, and a quota 429 drops the
/// credential's idle connections.
#[tokio::test]
async fn pooled_mode_scopes_connections_by_credential() {
    const RATE_LIMITED: &str = r#"{"error":{"code":429,"status":"RESOURCE_EXHAUSTED","details":[{"@type":"type.googleapis.com/google.rpc.ErrorInfo","reason":"RATE_LIMIT_EXCEEDED"},{"@type":"type.googleapis.com/google.rpc.RetryInfo","retryDelay":"30s"}]}}"#;
    let ok =
        r#"{"response":{"candidates":[{"content":{"role":"model","parts":[{"text":"ok"}]},"finishReason":"STOP"}]}}"#;
    let (base, connections, _) = keep_alive_upstream(vec![(200, ok), (200, ok), (200, ok), (429, RATE_LIMITED)]).await;
    let executor = AntigravityExecutor::default();
    let cfg = Config::parse("antigravity: {connection-pool: {enabled: true}}").unwrap();
    let count = || connections.load(std::sync::atomic::Ordering::SeqCst);
    let a = pooled_scenario(&base, "ag-pool-a");
    let b = pooled_scenario(&base, "ag-pool-b");
    executor.execute(&credential(&a, ""), request(&a), &cfg).await.unwrap();
    executor.execute(&credential(&a, ""), request(&a), &cfg).await.unwrap();
    assert_eq!(count(), 1, "credential A reuses its connection");
    executor.execute(&credential(&b, ""), request(&b), &cfg).await.unwrap();
    assert_eq!(count(), 2, "credential B has its own pool");
    let Err(error) = executor.execute(&credential(&a, ""), request(&a), &cfg).await else {
        panic!("expected the scripted 429");
    };
    assert_eq!(error.status, 429);
    assert_eq!(count(), 2, "the 429 came over A's pooled connection");
    // The short cooldown is per model; another model on A needs a new connection.
    let mut other = request(&a);
    other.model = "gemini-3.1-flash-lite".into();
    executor.execute(&credential(&a, ""), other, &cfg).await.unwrap();
    assert_eq!(count(), 3, "the 429 evicted A's idle connections");
}

/// TestAntigravityHomeModelThinkingLevelRemainsLevel: with the attempt's bound model
/// info (Go's Home `model_info`) listing thinking levels, an OpenAI `reasoning_effort`
/// reaches the upstream as that level, without a budget.
#[tokio::test]
async fn bound_level_model_keeps_the_thinking_level() {
    let answer = serde_json::json!({
        "status": 200,
        "headers": [["Content-Type", "application/json"]],
        "body": r#"{"response":{"candidates":[{"content":{"role":"model","parts":[{"text":"ok"}]},"finishReason":"STOP"}],"usageMetadata":{"promptTokenCount":1,"candidatesTokenCount":1,"totalTokenCount":2}}}"#,
    });
    let mock = Mock::start(&answer).await;
    let cred = credential(scenario("gemini_execute_basic"), mock.addr());
    let payload = r#"{"model":"gemini-3.8-flash-high","reasoning_effort":"medium","messages":[{"role":"user","content":"hello"}]}"#;
    let mut req = request(&serde_json::json!({
        "op": "execute", "source": "openai", "model": "gemini-3.8-flash-high", "payload": payload,
    }));
    let info = serde_json::json!({
        "id": "gemini-3.8-flash-high",
        "type": "gemini",
        "context_length": 1048576,
        "thinking": {"levels": ["low", "medium", "high"]},
        "user_defined": false,
    });
    req.resolved_model = Some(cpa_core::exec::ResolvedModel {
        info: cpa_core::registry::ModelInfo::from_raw(info.as_object().unwrap().clone()).unwrap(),
        source: cpa_core::exec::ResolvedSource::ApiKey,
    });
    let executor = AntigravityExecutor::default();
    executor.execute(&cred, req, &Config::parse("").unwrap()).await.unwrap();
    let wire = mock.request().unwrap();
    let body = wire.split_once("\r\n\r\n").unwrap().1.as_bytes().to_vec();
    let thinking = gj::get(&body, "request.generationConfig.thinkingConfig");
    assert_eq!(thinking.get("thinkingLevel").str(), "medium", "{wire}");
    assert!(!thinking.get("thinkingBudget").exists(), "{wire}");
}

/// TestPayloadBarrierAntigravityRebuiltAttempts (868e137): the payload rules run on the
/// finished envelope, so the built-in `maxOutputTokens` cleanup and the session ID
/// cannot undo them, and a filter applies once.
#[tokio::test]
async fn payload_rules_run_after_the_envelope() {
    let answer = serde_json::json!({
        "status": 200,
        "headers": [["Content-Type", "application/json"]],
        "body": r#"{"response":{"candidates":[{"content":{"role":"model","parts":[{"text":"ok"}]},"finishReason":"STOP"}]}}"#,
    });
    let cfg = Config::parse(
        r#"
payload:
  override:
    - models: [{name: "gemini-*"}]
      params: {"generationConfig.maxOutputTokens": 123}
  filter:
    - models: [{name: "gemini-*"}]
      params: ["sessionId", "contents.0"]
"#,
    )
    .unwrap();
    let mock = Mock::start(&answer).await;
    let cred = credential(scenario("gemini_execute_basic"), mock.addr());
    let payload =
        r#"{"contents":[{"role":"user","parts":[{"text":"first"}]},{"role":"user","parts":[{"text":"second"}]}]}"#;
    let req = request(&serde_json::json!({
        "op": "execute", "source": "gemini", "model": "gemini-3.1-pro-preview", "payload": payload,
    }));
    AntigravityExecutor::default().execute(&cred, req, &cfg).await.unwrap();
    let wire = mock.request().unwrap();
    let body = wire.split_once("\r\n\r\n").unwrap().1.as_bytes().to_vec();
    assert_eq!(
        gj::get(&body, "request.generationConfig.maxOutputTokens").int(),
        123,
        "{wire}"
    );
    assert!(!gj::get(&body, "request.sessionId").exists(), "{wire}");
    assert_eq!(gj::get(&body, "request.contents.#").int(), 1, "{wire}");
    assert_eq!(
        gj::get(&body, "request.contents.0.parts.0.text").str(),
        "second",
        "{wire}"
    );
}
