//! Kimi parity tests. Expected values come from the Go fixtures (see kimi_fixture) or
//! from Go's own test tables, never from this implementation.

use super::*;
use crate::kimi_fixture::{
    Captured, Mock, assert_same_request, credential, data_payloads, downstream, fixture, request,
};
use crate::kimi_http::{go_arch, go_os, hostname};

fn claude() -> ClaudeExecutor {
    ClaudeExecutor::with_client(wreq::Client::new(), crate::claude::DEFAULT_BASE_URL)
}

fn executor() -> KimiExecutor {
    KimiExecutor::with_client(wreq::Client::new())
}

fn cfg() -> Config {
    Config::parse("").unwrap()
}

/// Headers whose values depend on the machine or are random per run.
fn masked(fixture: &serde_json::Value) -> Vec<&'static str> {
    let mut masked = vec!["X-Msh-Device-Name", "X-Msh-Device-Model", "Referer"];
    if fixture["attributes"]["header:Host"].is_null() {
        masked.push("Host");
    }
    if fixture["credential"]["device_id"].as_str().is_none_or(str::is_empty) {
        masked.push("X-Msh-Device-Id");
    }
    masked
}

async fn run(name: &str) -> (serde_json::Value, Vec<Captured>, crate::kimi_fixture::Downstream) {
    let fx = fixture("kimi", name);
    let mock = Mock::start(&fx["responses"]).await;
    let cred = credential("kimi", &fx, Some(("base_url", format!("{}/coding", mock.url))));
    let exec = KimiExecutor::with_client(default_client());
    let cfg = Config::parse(fx["request"]["config"].as_str().unwrap_or_default()).unwrap();
    let result = exec.execute(&claude(), &cred, request(&fx, ""), &cfg).await;
    let down = downstream(result).await;
    (fx, mock.captured(), down)
}

#[tokio::test]
async fn upstream_requests_match_go_byte_for_byte() {
    for name in [
        "chat-nonstream-normalize",
        "chat-stream-suffix",
        "chat-error-429-clamped-none",
        "chat-disabled-thinking-temperature",
        "chat-kimi-ai-metadata-base",
        "responses-nonstream-reorder-suffix",
        "responses-stream-clamp",
        "chat-payload-rules",
        "responses-payload-rules",
        "responses-compact-rejected",
        "transport-custom-headers",
        "transport-redirect-307",
        "transport-gzip-response",
        "transport-explicit-gzip-not-decoded",
        "transport-json-typed-stream",
        "transport-stream-options-not-object",
    ] {
        let (fx, captured, _) = run(name).await;
        let go: Vec<Captured> = fx["upstream"]
            .as_array()
            .map(|u| u.iter().map(Captured::from_fixture).collect())
            .unwrap_or_default();
        assert_eq!(captured.len(), go.len(), "{name}: upstream request count");
        for (rust, go) in captured.iter().zip(&go) {
            assert_same_request(name, go, rust, &masked(&fx));
            assert_eq!(
                rust.header("X-Msh-Device-Name"),
                Some(hostname().unwrap_or_else(|| "unknown".into()).as_str())
            );
            assert_eq!(
                rust.header("X-Msh-Device-Model"),
                Some(format!("{} {}", go_os(), go_arch()).as_str())
            );
        }
    }
}

#[tokio::test]
async fn upstream_429_cools_the_model_without_a_retry_hint() {
    // kimi_executor.go returns statusErr{code, msg} for upstream errors: no retryAfter
    // (Retry-After is ignored) and not credential-scoped.
    let fx = fixture("kimi", "chat-error-429-clamped-none");
    let mock = Mock::start(&fx["responses"]).await;
    let cred = credential("kimi", &fx, Some(("base_url", format!("{}/coding", mock.url))));
    let error = executor()
        .execute(&claude(), &cred, request(&fx, ""), &cfg())
        .await
        .err()
        .unwrap();
    assert_eq!(error.status, 429);
    assert_eq!(error.scope, FailureScope::Model);
    assert_eq!(error.retry_after, None);
}

#[tokio::test]
async fn downstream_results_match_go() {
    for name in [
        "chat-nonstream-normalize",
        "chat-stream-suffix",
        "chat-error-429-clamped-none",
        "chat-disabled-thinking-temperature",
        "chat-kimi-ai-metadata-base",
        "responses-nonstream-reorder-suffix",
        "responses-stream-clamp",
        "responses-stream-data-only-frames",
        "responses-compact-rejected",
        "transport-custom-headers",
        "transport-redirect-307",
        "transport-gzip-response",
        "transport-json-typed-stream",
        "transport-stream-options-not-object",
    ] {
        let (fx, _, down) = run(name).await;
        let go = &fx["downstream"];
        // A plain Go error (status -1 in the fixture) is answered with 500 by the handler.
        if go["err_status"].as_i64() == Some(-1) {
            assert_eq!(down.err_status, Some(500), "{name}: error status");
            assert_eq!(down.err_body.as_deref(), go["err_body"].as_str(), "{name}: error body");
            continue;
        }
        if let Some(status) = go["err_status"].as_u64() {
            assert_eq!(down.err_status, Some(status as u16), "{name}: error status");
            assert_eq!(down.err_body.as_deref(), go["err_body"].as_str(), "{name}: error body");
            continue;
        }
        if let Some(body) = go["body"].as_str() {
            assert_eq!(down.body.as_deref(), Some(body), "{name}: body");
            continue;
        }
        let chunks: Vec<String> = go["chunks"]
            .as_array()
            .unwrap()
            .iter()
            .map(|c| c.as_str().unwrap().to_owned())
            .collect();
        if fx["request"]["source"] == "openai-response" {
            // Go emits each upstream line plus "\n" and its Responses route joins them into
            // frames (`frames`, recorded from Go's responsesSSEFramer before route repair).
            let frames: Vec<String> = go["frames"]
                .as_array()
                .unwrap()
                .iter()
                .map(|c| c.as_str().unwrap().to_owned())
                .collect();
            assert_eq!(down.chunks, frames, "{name}: stream frames");
        } else {
            // Go's chat handler writes each non-empty translated chunk as `data: %s\n\n`.
            let go_frames: Vec<String> = chunks
                .into_iter()
                .filter(|c| !c.is_empty())
                .map(|c| format!("data: {c}\n\n"))
                .collect();
            assert_eq!(down.chunks, go_frames, "{name}: stream frames");
        }
        assert!(down.err_status.is_none(), "{name}: unexpected stream error");
    }
}

#[tokio::test]
async fn explicit_accept_encoding_returns_raw_gzip_like_go() {
    // Go decodes only gzip its transport asked for; a configured Accept-Encoding passes
    // the compressed bytes through unchanged.
    let (fx, _, down) = run("transport-explicit-gzip-not-decoded").await;
    assert_eq!(down.raw, crate::kimi_fixture::response_body(&fx, 0));
    assert_eq!(&down.raw[..2], &[0x1f, 0x8b]);
}

#[tokio::test]
async fn claude_delegation_keeps_kimi_owned_wire_parts() {
    // Kimi owns the base URL, model normalization, token and response-model restoration.
    // The rest of the Messages wire (headers, cache_control, stream flag) is the Claude
    // executor's; full parity is `claude_delegation_full_wire_parity` below.
    let (fx, captured, down) = run("claude-nonstream-delegated").await;
    let go = Captured::from_fixture(&fx["upstream"][0]);
    assert_eq!(captured.len(), 1);
    assert_eq!(captured[0].target, go.target);
    assert_eq!(captured[0].header("Authorization"), go.header("Authorization"));
    assert_eq!(
        gj::get(&captured[0].body, "model").str(),
        gj::get(&go.body, "model").str()
    );
    assert_eq!(
        down.body.as_deref(),
        fx["downstream"]["body"].as_str(),
        "model restored to the client's"
    );
}

#[tokio::test]
async fn claude_delegation_full_wire_parity() {
    let (fx, captured, _) = run("claude-nonstream-delegated").await;
    assert_same_request(
        "claude-nonstream-delegated",
        &Captured::from_fixture(&fx["upstream"][0]),
        &captured[0],
        &["Host"],
    );
}

#[tokio::test]
async fn count_tokens_goes_upstream_like_go() {
    let (fx, captured, down) = run("claude-count-tokens-upstream").await;
    assert_same_request(
        "claude-count-tokens-upstream",
        &Captured::from_fixture(&fx["upstream"][0]),
        &captured[0],
        &["Host"],
    );
    assert_eq!(down.body.as_deref(), fx["downstream"]["body"].as_str());
}

#[tokio::test]
async fn refresh_requests_and_patches_match_go() {
    for name in [
        "refresh-rotates",
        "refresh-ai-keeps-refresh",
        "refresh-rejected",
        "refresh-500",
    ] {
        let fx = fixture("kimi", name);
        let mock = Mock::start(&fx["responses"]).await;
        let mut cred = credential("kimi", &fx, None);
        cred.provider = fx["credential"]["type"].as_str().unwrap().into();
        let exec = executor().with_oauth_host(&mock.url);
        assert!(
            exec.needs_prepare(&cred, &cfg()),
            "{name}: no expiry and no last_refresh means refresh"
        );
        let before = chrono::Utc::now().timestamp();
        let result = exec.prepare(&cred, &cfg()).await;
        let go = Captured::from_fixture(&fx["upstream"][0]);
        let rust = &mock.captured()[0];
        let mut mask = vec!["Host", "X-Msh-Device-Name", "X-Msh-Device-Model"];
        if fx["credential"]["device_id"].is_null() {
            mask.push("X-Msh-Device-Id");
        }
        assert_same_request(name, &go, rust, &mask);
        assert_eq!(rust.header("Host"), Some(mock.url.trim_start_matches("http://")));
        match fx["extra"]["metadata"].as_object() {
            None => {
                let error = result.expect_err(name);
                let go_message = fx["downstream"]["err_body"].as_str().unwrap();
                // Go appends the response body after "status N: "; it is withheld here.
                let message = String::from_utf8_lossy(&error.body).into_owned();
                assert!(
                    go_message.starts_with(&message),
                    "{name}: {message:?} vs {go_message:?}"
                );
                assert_eq!(error.status, fx["responses"][0]["status"].as_u64().unwrap() as u16);
            }
            Some(go_meta) => {
                let mut meta = cred.metadata.clone();
                result.unwrap().apply(&mut meta);
                let sorted =
                    |m: &serde_json::Map<String, Value>| m.keys().cloned().collect::<std::collections::BTreeSet<_>>();
                assert_eq!(sorted(&meta), sorted(go_meta), "{name}: keys");
                for (key, value) in go_meta {
                    match key.as_str() {
                        "last_refresh" => assert!(crate::kimi_http::parse_time(&meta[key]).is_some()),
                        "expired" => {
                            let expired = crate::kimi_http::parse_time(&meta[key]).unwrap().timestamp();
                            assert!(
                                (before + 3600..=before + 3605).contains(&expired),
                                "{name}: expired {expired}"
                            );
                            let go_before = fx["extra"]["before_unix"].as_i64().unwrap();
                            let go_expired = crate::kimi_http::parse_time(value).unwrap().timestamp();
                            assert!((go_before + 3600..=go_before + 3605).contains(&go_expired));
                        }
                        _ => assert_eq!(&meta[key], value, "{name}: {key}"),
                    }
                }
            }
        }
    }
}

#[tokio::test]
async fn concurrent_refreshes_share_one_exchange() {
    // Go: TestRefreshToken_DeduplicatesConcurrentRefreshAcrossInstances.
    let responses = serde_json::json!([{"status":200,"headers":[["Content-Type","application/json"]],
        "body":"{\"access_token\":\"once\",\"refresh_token\":\"r2\",\"expires_in\":3600}"}]);
    let mock = Mock::start(&responses).await;
    let flows: Vec<_> = (0..4)
        .map(|_| DeviceFlow::new(wreq::Client::new(), "kimi.com", "d").with_oauth_host(&mock.url))
        .collect();
    let results = futures_util::future::join_all(flows.iter().map(|f| f.refresh("shared-refresh"))).await;
    assert!(results.iter().all(|r| r.as_ref().unwrap().access_token == "once"));
    assert_eq!(mock.captured().len(), 1);
}

#[tokio::test]
async fn login_writes_go_identical_credential_file() {
    for (name, provider) in [("login-kimi", "kimi"), ("login-kimi-ai", "kimi-ai")] {
        let fx = fixture("kimi", name);
        let mock = Mock::start(&fx["responses"]).await;
        let dir = std::env::temp_dir().join(format!("kimi-login-{}", uuid::Uuid::new_v4()));
        let domain = if provider == "kimi" { "kimi.com" } else { "kimi.ai" };
        let flow = DeviceFlow::new(wreq::Client::new(), domain, "")
            .with_oauth_host(&mock.url)
            .with_min_interval(std::time::Duration::from_millis(1));
        let path = kimi_auth::login_with(flow, provider, &dir, true).await.unwrap();
        let captured = mock.captured();
        let go: Vec<Captured> = fx["upstream"]
            .as_array()
            .unwrap()
            .iter()
            .map(Captured::from_fixture)
            .collect();
        assert_eq!(
            captured.len(),
            go.len(),
            "{name}: device code, pending poll, success poll"
        );
        for (rust, go) in captured.iter().zip(&go) {
            assert_same_request(
                name,
                go,
                rust,
                &["Host", "X-Msh-Device-Name", "X-Msh-Device-Model", "X-Msh-Device-Id"],
            );
        }
        let device_ids: std::collections::HashSet<_> =
            captured.iter().map(|c| c.header("X-Msh-Device-Id").unwrap()).collect();
        assert_eq!(device_ids.len(), 1, "one device identity per login");

        let written = std::fs::read_to_string(&path).unwrap();
        let go_file = fx["extra"]["file"].as_object().unwrap();
        // The encoder reproduces Go's exact bytes for Go's own map.
        assert_eq!(
            kimi_auth::encode_credential(go_file),
            fx["extra"]["file_raw"].as_str().unwrap()
        );
        let ours: serde_json::Map<String, Value> = serde_json::from_str(&written).unwrap();
        assert_eq!(ours.keys().collect::<Vec<_>>(), go_file.keys().collect::<Vec<_>>());
        for (key, value) in go_file {
            match key.as_str() {
                "device_id" => assert_eq!(ours[key].as_str(), captured[0].header("X-Msh-Device-Id")),
                "timestamp" | "expired" => {}
                _ => assert_eq!(&ours[key], value, "{name}: {key}"),
            }
        }
        let timestamp = ours["timestamp"].as_i64().unwrap();
        assert_eq!(
            path.file_name().unwrap().to_str().unwrap(),
            format!("{provider}-{timestamp}.json")
        );
        let expired = crate::kimi_http::parse_time(&ours["expired"]).unwrap().timestamp();
        assert!((timestamp / 1000 + 3599..=timestamp / 1000 + 3601).contains(&expired));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }
}

use serde_json::Value;

#[test]
fn upstream_model_normalization_matches_go_table() {
    // TestNormalizeKimiUpstreamModel (kimi_executor_test.go).
    for (input, want) in [
        ("kimi-k3[1m]", "k3"),
        ("Kimi-K3[1M]", "k3"),
        ("k3", "k3"),
        ("kimi-k2.6[1m]", "k2.6"),
        ("kimi-k3[1m](1024)", "k3(1024)"),
        ("kimi-k2.6(high)", "k2.6(high)"),
        ("kimi-k2.7-code", "kimi-for-coding"),
        ("Kimi-K2.7-Code", "kimi-for-coding"),
        ("kimi-k2.7-code-highspeed(high)", "kimi-for-coding-highspeed(high)"),
        ("kimi-k2.7-code[1m](high)", "kimi-for-coding(high)"),
        ("k2.8-code", "kimi-for-coding"),
        ("kimi-k2.8-preview", "kimi-for-coding"),
        ("kimi-k2.8(max)", "kimi-for-coding(max)"),
        ("Kimi-For-Coding", "kimi-for-coding"),
        ("kimi-for-coding[1m]", "kimi-for-coding"),
        ("for-coding-highspeed", "kimi-for-coding-highspeed"),
    ] {
        assert_eq!(normalize_upstream_model(input), want, "{input}");
    }
}

#[test]
fn urls_follow_go_resolution() {
    // TestResolveKimiResponsesURL / ChatURL / ClaudeBaseURL (kimi_responses_test.go).
    let cred = |base: &str| {
        let mut c = Credential::from_file(
            std::path::Path::new("/a"),
            std::path::Path::new("/a/k.json"),
            serde_json::json!({"type":"kimi"}).as_object().unwrap().clone(),
        )
        .unwrap();
        if !base.is_empty() {
            c.attributes.insert("base_url".into(), base.into());
        }
        c
    };
    assert_eq!(responses_url(&cred("")), "https://api.kimi.com/coding/v1/responses");
    assert_eq!(
        responses_url(&cred("https://api.kimi.com/coding/")),
        "https://api.kimi.com/coding/v1/responses"
    );
    assert_eq!(
        responses_url(&cred("https://api.kimi.com/coding/v1")),
        "https://api.kimi.com/coding/v1/responses"
    );
    assert_eq!(
        chat_url(&cred("https://api.kimi.ai/coding/v1/")),
        "https://api.kimi.ai/coding/v1/chat/completions"
    );
    assert_eq!(
        claude_base_url(&cred("https://api.kimi.ai/coding/v1")),
        "https://api.kimi.ai/coding"
    );
    assert_eq!(claude_base_url(&cred("")), "https://api.kimi.com/coding");
    let mut ai = cred("");
    ai.provider = "kimi-ai".into();
    ai.metadata.insert("type".into(), "kimi-ai".into());
    assert_eq!(chat_url(&ai), "https://api.kimi.ai/coding/v1/chat/completions");
}

#[test]
fn pure_functions_match_go_vectors() {
    // vectors.json: inputs run through the Go functions by zz_rsfix_vectors_test.go.
    let path = format!("{}/tests/device_fixtures/kimi/vectors.json", env!("CARGO_MANIFEST_DIR"));
    let vectors: Vec<Value> = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
    assert!(vectors.len() >= 30);
    for v in &vectors {
        let input = v["in"].as_str().unwrap();
        let want = v["out"].as_str().unwrap();
        let err = v["err"].as_str();
        let bytes = input.as_bytes();
        let text = |out: Vec<u8>| String::from_utf8(out).unwrap();
        let path = || v["path"].as_str().unwrap();
        let value = || v["value"].as_str().unwrap_or_default();
        let got: Result<String, String> = match v["fn"].as_str().unwrap() {
            "schema" => Ok(text(normalize_parameters_schema(bytes))),
            "links" => normalize_tool_message_links(bytes.to_vec())
                .map(text)
                .map_err(|e| String::from_utf8_lossy(&e.body).into_owned()),
            "temperature" => Ok(text(normalize_temperature(bytes.to_vec()))),
            "responses_input" => Ok(text(normalize_responses_input(bytes.to_vec()))),
            "thinking" => {
                let (from, to) = (v["from"].as_str().unwrap(), v["to"].as_str().unwrap());
                // Go's registry: only the Codex target registers translators from these.
                let has_request_transformer = to == "codex" && matches!(from, "openai" | "openai-response");
                apply_request_thinking(&RequestThinking {
                    body: bytes,
                    payload: bytes,
                    original: bytes,
                    model: v["model"].as_str().unwrap(),
                    from,
                    to,
                    provider: "kimi",
                    resolved: None,
                    has_request_transformer,
                    updates_changed: false,
                })
                .map(text)
                .map_err(|e| e.message)
            }
            "restore_model" => Ok(text(
                restore_response_model(bytes, v["model"].as_str().unwrap()).to_vec(),
            )),
            "sjson_delete" => Ok(text(gj::try_delete(bytes, path()).unwrap_or_else(|_| bytes.to_vec()))),
            "sjson_set_str" => gj::try_set_str(bytes, path(), value()).map(text),
            "sjson_set_raw" => gj::try_set_raw(bytes, path(), value()).map(text),
            "gjson_string" => Ok(text(gj::get(bytes, "n").bytes().into_owned())),
            other => panic!("unknown vector {other}"),
        };
        match err {
            Some(message) => assert_eq!(got, Err(message.to_owned()), "{v}"),
            None => assert_eq!(got, Ok(want.to_owned()), "{v}"),
        }
    }
}

#[tokio::test]
async fn claude_replay_sequence_matches_go() {
    // Six Claude-format turns on one executor (zz_rsfix_kimi_test.go TestRSFixKimiReplay):
    // cache from JSON and from SSE, replay across K3 variants, clear after a 400, and no
    // replay into another model family. The replayed assistant turn is Kimi-owned; the rest
    // of each body belongs to the Claude executor.
    let fx = fixture("kimi", "claude-replay-sequence");
    let mock = Mock::start(&fx["responses"]).await;
    let cred = credential("kimi", &fx, Some(("base_url", format!("{}/coding", mock.url))));
    let exec = executor();
    let claude = claude();
    let steps = fx["request"]["steps"].as_array().unwrap();
    let go_down = fx["extra"]["downstream"].as_array().unwrap();
    for (i, step) in steps.iter().enumerate() {
        let model = step["model"].as_str().unwrap();
        let body = Bytes::from(
            step["body"]
                .as_str()
                .unwrap()
                .replacen("\"M\"", &format!("\"{model}\""), 1),
        );
        let mut headers = http::HeaderMap::new();
        headers.insert("x-claude-code-session-id", "sess-1".parse().unwrap());
        let req = ExecRequest {
            operation: Operation::Generate,
            source_format: Format::Claude,
            response_format: Format::Claude,
            requested_model: model.into(),
            model: model.into(),
            original_body: body.clone(),
            body,
            stream: step["stream"].as_bool().unwrap(),
            alt: None,
            session: None,
            execution_session: None,
            derived_session: None,
            resolved_model: None,
            request_path: String::new(),
            headers,
            caller: cpa_core::exec::Caller {
                principal: "client-key-1".into(),
                source: "authorization",
            },
        };
        let down = downstream(exec.execute(&claude, &cred, req, &cfg()).await).await;
        let go = &go_down[i];
        match go["err_status"].as_u64() {
            Some(status) => assert_eq!(down.err_status, Some(status as u16), "step {i}"),
            None if step["stream"].as_bool().unwrap() => {
                let go_chunks: Vec<String> = go["chunks"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|c| c.as_str().unwrap().to_owned())
                    .collect();
                assert_eq!(data_payloads(&down.chunks), data_payloads(&go_chunks), "step {i}");
            }
            None => assert_eq!(down.body.as_deref(), go["body"].as_str(), "step {i}"),
        }
    }
    let captured = mock.captured();
    let go_upstream = fx["upstream"].as_array().unwrap();
    assert_eq!(captured.len(), go_upstream.len());
    for (i, (rust, go)) in captured.iter().zip(go_upstream).enumerate() {
        let go = Captured::from_fixture(go);
        let content = |c: &Captured| gj::canonical(gj::get(&c.body, "messages.1.content").raw());
        assert_eq!(content(rust), content(&go), "step {i}: replayed assistant content");
        assert_eq!(
            gj::get(&rust.body, "model").str(),
            gj::get(&go.body, "model").str(),
            "step {i}: model"
        );
    }
}

#[tokio::test]
async fn redirect_hop_carries_previous_url_as_referer() {
    let (_, captured, _) = run("transport-redirect-307").await;
    assert_eq!(captured.len(), 2);
    let host = captured[0].header("Host").unwrap();
    assert_eq!(captured[1].target, "/coding/v1/chat/completions-next");
    assert_eq!(
        captured[1].header("Referer"),
        Some(format!("http://{host}/coding/v1/chat/completions").as_str())
    );
    assert_eq!(captured[1].body, captured[0].body, "307 resends the body");
}
