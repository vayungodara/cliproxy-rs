//! Devin parity tests. Expected values come from the Go fixtures
//! (tests/device_fixtures/devin, produced by the real Go executor, auth service and
//! helpers), never from this implementation.

use super::*;
use crate::devin_auth::{DevinAuth, parse_manual_paste, parse_user_status, save_record, user_status_request};
use crate::devin_request::{Parsed, parse_signature};
use crate::devin_wire::{Prompt, device_fingerprint, pb, sanitize_system_prompt, sanitize_tool_description};
use crate::kimi_fixture::{Captured, Mock, downstream, fixture, request};
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use serde_json::json;

fn vectors() -> Value {
    fixture("devin", "vectors")
}

fn b64(raw: &[u8]) -> String {
    STANDARD.encode(raw)
}

#[test]
fn catalog_matches_go_get_devin_models() {
    let v = vectors();
    let rust: Vec<Value> = cpa_core::registry::devin::models()
        .into_iter()
        .map(|m| Value::Object(m.raw))
        .collect();
    assert_eq!(Value::Array(rust), v["catalog"]);
    for case in v["lookup"].as_array().unwrap() {
        let id = case["id"].as_str().unwrap();
        let found = cpa_core::registry::devin::lookup(id).map(|m| m.id);
        assert_eq!(found.as_deref(), case["found"].as_str(), "LookupDevinModel({id:?})");
    }
    for case in v["static_lookup"].as_array().unwrap() {
        let id = case["id"].as_str().unwrap();
        let found = cpa_core::registry::pinned().lookup(id);
        assert_eq!(
            found.map(|m| m.kind.clone()).as_deref(),
            case["type"].as_str(),
            "LookupStaticModelInfo({id:?}) type"
        );
        if let Some(m) = found {
            assert_eq!(
                m.raw.get("max_completion_tokens"),
                Some(&case["max_completion_tokens"]),
                "{id}"
            );
        }
    }
}

#[test]
fn catalog_validation_matches_go() {
    for case in vectors()["validate"].as_array().unwrap() {
        let input = case["input"].as_str().unwrap();
        match cpa_core::registry::devin::validate(input.as_bytes()) {
            Ok(models) => {
                let rust: Vec<Value> = models.into_iter().map(|m| Value::Object(m.raw)).collect();
                assert_eq!(Value::Array(rust), case["models"], "{input}");
            }
            Err(e) => assert_eq!(Some(e.as_str()), case["error"].as_str(), "{input}"),
        }
    }
}

#[test]
fn catalog_store_loads_like_go() {
    let store = cpa_core::registry::devin::Store::empty();
    let raw = br#"{"devin":[{"id":"x-1"},{"id":"x-1-high"}]}"#;
    assert_eq!(store.load(raw, "test"), Ok(true));
    assert_eq!(store.load(raw, "test"), Ok(false), "same bytes are no change");
    assert_eq!(store.snapshot().1, 1);
    let ids: Vec<String> = store.models().into_iter().map(|m| m.id).collect();
    assert_eq!(ids, ["devin/x-1", "devin/swe-1-6-slow"]);
    assert_eq!(store.lookup("X-1-LOW").map(|m| m.id).as_deref(), Some("devin/x-1"));
    assert!(
        store
            .load(b"[]", "remote")
            .unwrap_err()
            .starts_with("remote: invalid Devin models JSON")
    );
    assert_eq!(store.snapshot().1, 1, "a rejected catalog keeps the current one");
}

#[test]
fn model_uids_match_go() {
    for case in vectors()["resolve_uid"].as_array().unwrap() {
        let (model, level, budget) = (
            case["model"].as_str().unwrap(),
            case["level"].as_str().unwrap_or_default(),
            case["budget"].as_i64().unwrap_or(0),
        );
        assert_eq!(
            resolve_chat_model_uid(model, level, budget),
            case["want"].as_str().unwrap(),
            "ResolveDevinChatModelUID({model:?}, {level:?}, {budget})"
        );
    }
}

#[test]
fn trailers_match_go() {
    for case in vectors()["trailers"].as_array().unwrap() {
        let input = case["input"].as_str().unwrap();
        let got = parse_trailer_error(input.as_bytes());
        assert_eq!(
            got.as_ref().map_or(0, |(code, _)| i64::from(*code)),
            case["code"].as_i64().unwrap(),
            "{input}"
        );
        assert_eq!(got.map(|(_, m)| m).as_deref(), case["error"].as_str(), "{input}");
    }
}

#[test]
fn sanitizers_match_go() {
    let v = vectors();
    let words: Vec<String> = ["secret", " Ab ", "x", "zero\u{200b}width", "Ünïcode"]
        .iter()
        .map(|s| (*s).to_owned())
        .collect();
    let matcher = SensitiveWords::new(&words).unwrap();
    for case in v["sanitize"].as_array().unwrap() {
        let input = case["input"].as_str().unwrap().as_bytes();
        assert_eq!(
            String::from_utf8(sanitize_system_prompt(input, None)).unwrap(),
            case["plain"].as_str().unwrap()
        );
        assert_eq!(
            String::from_utf8(sanitize_system_prompt(input, Some(&matcher))).unwrap(),
            case["matched"].as_str().unwrap()
        );
    }
    for case in v["tool_descriptions"].as_array().unwrap() {
        let got = sanitize_tool_description(
            case["name"].as_str().unwrap().as_bytes(),
            case["input"].as_str().unwrap().as_bytes(),
        );
        assert_eq!(String::from_utf8(got).unwrap(), case["want"].as_str().unwrap());
    }
}

#[test]
fn signatures_match_go() {
    for case in vectors()["signatures"].as_array().unwrap() {
        let input = case["input"].as_str().unwrap();
        let (bytes, kind) = parse_signature(input.as_bytes());
        assert_eq!(b64(&bytes), case["bytes_b64"].as_str().unwrap(), "{input:?}");
        assert_eq!(kind, case["type"].as_str().unwrap(), "{input:?}");
    }
}

#[test]
fn identities_and_urls_match_go() {
    let v = vectors();
    assert_eq!(
        device_fingerprint("seed-1"),
        v["fingerprints"]["seed-1"].as_str().unwrap()
    );
    assert_eq!(device_fingerprint("tok"), v["fingerprints"]["tok"].as_str().unwrap());
    assert_eq!(device_fingerprint("").len(), 732);
    assert_ne!(device_fingerprint(""), device_fingerprint(""), "random without a seed");
    for case in v["uuids"].as_array().unwrap() {
        assert_eq!(
            normalize_uuid(case["input"].as_str().unwrap()),
            case["want"].as_str().unwrap()
        );
    }
    let auth = DevinAuth::new(wreq::Client::new());
    let urls = [
        auth.build_authorization_url("http://127.0.0.1:5555/callback", "chal+/=", "st ate"),
        auth.build_authorization_url("  ", "chal", ""),
        auth.build_authorization_url("", "c", "s"),
    ];
    assert_eq!(json!(urls), v["auth_urls"]);
    for (input, want) in v["format_tokens"].as_object().unwrap() {
        assert_eq!(crate::devin_auth::format_session_token(input), want.as_str().unwrap());
    }
    for case in fixture("devin", "paste_vectors").as_array().unwrap() {
        let input = case["input"].as_str().unwrap();
        let got = parse_manual_paste(input, "st");
        let (code, token) = got.clone().unwrap_or_default();
        assert_eq!(code, case["code"].as_str().unwrap(), "{input:?}");
        assert_eq!(token, case["token"].as_str().unwrap(), "{input:?}");
        assert_eq!(got.err().as_deref(), case["error"].as_str(), "{input:?}");
    }
}

#[test]
fn user_status_matches_go() {
    let v = vectors();
    let raw = STANDARD
        .decode(v["user_status"]["input_b64"].as_str().unwrap())
        .unwrap();
    let s = parse_user_status(&raw).unwrap();
    let time = |t: Option<chrono::DateTime<chrono::Utc>>| {
        t.map_or("0001-01-01T00:00:00Z".to_owned(), |t| {
            t.format("%Y-%m-%dT%H:%M:%SZ").to_string()
        })
    };
    let go = &v["user_status"]["parsed"];
    assert_eq!(
        json!({
            "email": s.email, "user_name": s.user_name, "user_id": s.user_id, "team_id": s.team_id,
            "org_id": s.org_id, "org_name": s.org_name, "plan": s.plan,
            "daily_quota_remaining_percent": s.daily_quota_remaining_percent,
            "weekly_quota_remaining_percent": s.weekly_quota_remaining_percent,
            "daily_quota_reset_at": time(s.daily_quota_reset_at),
            "weekly_quota_reset_at": time(s.weekly_quota_reset_at),
            "plan_start": time(s.plan_start), "plan_end": time(s.plan_end),
        }),
        *go
    );
    if go_os_matches_fixture() {
        assert_eq!(
            b64(&user_status_request("tok", "fp")),
            v["user_status_request_b64"].as_str().unwrap()
        );
    }
}

#[test]
fn utf8_split_matches_go() {
    let mut buf = Utf8Split::default();
    for case in vectors()["utf8_feeds"].as_array().unwrap() {
        let input = STANDARD.decode(case["in_b64"].as_str().unwrap()).unwrap();
        assert_eq!(b64(&buf.feed(&input)), case["out_b64"].as_str().unwrap());
    }
}

/// `DevinFrameResult` as Go's encoding/json renders it.
fn frame_json(f: &crate::devin_wire::Frame) -> Value {
    let s = |b: &[u8]| Value::String(String::from_utf8_lossy(b).into_owned());
    let bytes = |b: &[u8]| {
        if b.is_empty() {
            Value::Null
        } else {
            Value::String(b64(b))
        }
    };
    let tool_calls: Vec<Value> = f
        .tool_calls
        .iter()
        .map(|t| {
            json!({"ID": s(&t.id), "Name": s(&t.name), "Arguments": s(&t.arguments),
                "InvalidJSONStr": s(&t.invalid_json), "InvalidJSONErr": s(&t.invalid_json_error),
                "IsCustomToolCall": t.is_custom})
        })
        .collect();
    let usage = f.usage.as_ref().map_or(Value::Null, |u| {
        let mut m = serde_json::Map::new();
        m.insert("prompt_tokens".into(), u.prompt_tokens.into());
        m.insert("completion_tokens".into(), u.completion_tokens.into());
        m.insert("cached_tokens".into(), u.cached_tokens.into());
        if u.cache_write_tokens != 0 {
            m.insert("cache_write_tokens".into(), u.cache_write_tokens.into());
        }
        if u.status_code != 0 {
            m.insert("status_code".into(), u.status_code.into());
        }
        if !u.request_id.is_empty() {
            m.insert("request_id".into(), s(&u.request_id));
        }
        if !u.model_name.is_empty() {
            m.insert("model_name".into(), s(&u.model_name));
        }
        if !u.headers.is_empty() {
            let h: serde_json::Map<String, Value> = u
                .headers
                .iter()
                .map(|(k, v)| (String::from_utf8_lossy(k).into_owned(), s(v)))
                .collect();
            m.insert("headers".into(), Value::Object(h));
        }
        Value::Object(m)
    });
    json!({
        "OutputID": s(&f.output_id), "Timestamp": f.timestamp, "ContentText": s(&f.content),
        "DeltaTokens": f.delta_tokens, "StopReason": f.stop_reason,
        "ToolCallDeltas": if tool_calls.is_empty() { Value::Null } else { Value::Array(tool_calls) },
        "ThinkingText": s(&f.thinking), "DeltaSignature": bytes(&f.signature),
        "DeltaSignatureType": s(&f.signature_type), "Latency": f.latency, "MessageID": s(&f.message_id),
        "Usage": usage,
        "ResponseDimensionGroups": if f.dimension_groups.is_empty() { Value::Null } else {
            Value::Array(f.dimension_groups.iter().map(|g| Value::String(b64(g))).collect()) },
        "UnknownFieldNumbers": if f.unknown_fields.is_empty() { Value::Null } else { json!(f.unknown_fields) },
    })
}

#[test]
fn frames_parse_like_go() {
    for case in vectors()["frames"].as_array().unwrap() {
        let raw = STANDARD.decode(case["input_b64"].as_str().unwrap()).unwrap();
        let (frame, error) = parse_frame(&raw);
        let mut got = frame_json(&frame);
        let mut want = case["result"].clone();
        // Go encodes 1.25 as 1.25 and 0 as 0; compare numerically.
        for v in [&mut got, &mut want] {
            v["Latency"] = json!(v["Latency"].as_f64().unwrap());
        }
        if error.is_none() {
            assert_eq!(got, want, "{}", case["input_b64"]);
        }
        assert_eq!(error.as_deref(), case["error"].as_str(), "{}", case["input_b64"]);
    }
    // Go keeps the partial result on error; callers skip such frames anyway.
}

/// `parseInteractionsPayload` output in the shape the Go generator recorded.
fn parsed_json(p: &Parsed) -> Value {
    let s = |b: &[u8]| Value::String(String::from_utf8_lossy(b).into_owned());
    let bytes = |b: &[u8]| {
        if b.is_empty() {
            Value::Null
        } else {
            Value::String(b64(b))
        }
    };
    let prompt = |p: &Prompt| {
        let images: Vec<Value> = p
            .images
            .iter()
            .map(|i| json!({"Base64Data": s(&i.base64), "MimeType": s(&i.mime)}))
            .collect();
        let calls: Vec<Value> = p
            .tool_calls
            .iter()
            .map(|c| json!({"ID": s(&c.id), "Name": s(&c.name), "Arguments": s(&c.arguments)}))
            .collect();
        json!({
            "MessageID": "", "Source": p.source, "Content": s(&p.content),
            "Images": if images.is_empty() { Value::Null } else { Value::Array(images) },
            "ToolCalls": if calls.is_empty() { Value::Null } else { Value::Array(calls) },
            "ToolCallID": s(&p.tool_call_id), "OriginalToolCallID": s(&p.original_tool_call_id),
            "IsOrphanedTool": p.is_orphaned_tool, "Thinking": s(&p.thinking),
            "Signature": bytes(&p.signature), "SignatureType": s(&p.signature_type),
        })
    };
    let tools: Vec<Value> = p
        .tools
        .iter()
        .map(|t| json!({"Name": s(&t.name), "Description": s(&t.description), "Parameters": bytes(&t.parameters)}))
        .collect();
    let prompts: Vec<Value> = p.prompts.iter().map(prompt).collect();
    let mut out = json!({
        "system": s(&p.system),
        "prompts": if prompts.is_empty() { Value::Null } else { Value::Array(prompts) },
        "tools": if tools.is_empty() { Value::Null } else { Value::Array(tools) },
        "max_tokens": p.max_tokens, "session_id": s(&p.session_id), "cascade_id": s(&p.cascade_id),
        "level": p.thinking_level, "budget": p.budget,
    });
    if let Some(t) = p.temperature {
        out["temperature"] = json!(t);
    }
    out
}

#[test]
fn interactions_payloads_parse_like_go() {
    for case in vectors()["parse_interactions"].as_array().unwrap() {
        let parsed = crate::devin_request::parse_interactions(
            case["payload"].as_str().unwrap().as_bytes(),
            case["original"].as_str().unwrap().as_bytes(),
        );
        let mut want = case.clone();
        let obj = want.as_object_mut().unwrap();
        obj.remove("payload");
        obj.remove("original");
        assert_eq!(parsed_json(&parsed), want, "{}", case["payload"]);
    }
}

/// Overwrites the bytes of nested protobuf string fields (`path` of field numbers) with
/// `X`, so random values of equal length compare equal.
fn mask_proto(msg: &mut [u8], path: &[i64]) {
    let mut pos = 0;
    while pos < msg.len() {
        let Ok((num, wire, n)) = pb::consume_tag(&msg[pos..]) else {
            return;
        };
        pos += n;
        if wire != pb::BYTES {
            let Ok(n) = pb::consume_field_value(num, wire, &msg[pos..]) else {
                return;
            };
            pos += n;
            continue;
        }
        let Ok((len, n)) = pb::consume_varint(&msg[pos..]) else {
            return;
        };
        let start = pos + n;
        let end = start + len as usize;
        if num == path[0] {
            if path.len() == 1 {
                msg[start..end].fill(b'X');
            } else {
                mask_proto(&mut msg[start..end], &path[1..]);
            }
        }
        pos = end;
    }
}

/// Go fills `runtime.GOOS` into client metadata; the fixtures were recorded on Linux.
fn go_os_matches_fixture() -> bool {
    crate::kimi_http::go_os() == "linux"
}

fn bind_session(req: &mut ExecRequest) {
    let id = cpa_common::session::extract_session_id(
        &req.headers,
        &req.original_body,
        &cpa_common::session::Meta::default(),
    );
    req.session = (!id.is_empty()).then(|| cpa_common::session::bound_session_identity(&id));
}

/// Replaces the random parts Go and Rust both generate: interaction IDs, `created`
/// timestamps and RFC 3339 times.
fn mask_output(text: &str) -> String {
    let re =
        regex::Regex::new(r#"interaction_[0-9a-f-]{12}|"created(_at)?":\d+|\d{4}-\d\d-\d\dT\d\d:\d\d:\d\d(\.\d+)?Z"#)
            .unwrap();
    re.replace_all(text, "<masked>").into_owned()
}

const EXECUTOR_FIXTURES: [&str; 31] = [
    "chat-nonstream",
    "chat-stream",
    "responses-nonstream",
    "responses-stream",
    "claude-nonstream",
    "claude-stream",
    "interactions-nonstream",
    "interactions-stream-gzip",
    "gemini-stream",
    "gemini-nonstream-dimension-usage",
    "chat-stream-trailer-before-content",
    "chat-stream-trailer-after-content",
    "responses-stream-trailer-after-content",
    "chat-nonstream-trailer-credit",
    "chat-nonstream-trailer-internal-invalid",
    "chat-http-429-retry-after",
    "chat-stream-http-500",
    "chat-stream-premature-eof",
    "chat-nonstream-premature-eof",
    "chat-stream-bad-frame-flag",
    "chat-stream-max-tokens",
    "chat-nonstream-content-filter",
    "chat-sensitive-words",
    "responses-apply-patch-legacy",
    "responses-apply-patch-trailer",
    "chat-custom-headers-attrs",
    "interactions-stream-ordering",
    "responses-stream-ordering",
    "interactions-nonstream-ordering",
    "count-tokens",
    "missing-credentials",
];

/// Runs one executor fixture once (turn indexes are process-wide per session, so every
/// fixture has its own session and runs exactly once): the upstream requests and the
/// downstream result must match Go's.
async fn check_executor_fixture(name: &str) {
    let fx = fixture("devin", name);
    let mock = Mock::start(&fx["responses"]).await;
    let mut cred = crate::kimi_fixture::credential("devin", &fx, None);
    cred.id = "devin-fixture.json".into();
    if cred.attributes.contains_key("base_url") {
        cred.attributes.insert("base_url".into(), mock.url.clone());
    } else if cred.metadata.contains_key("base_url") {
        cred.metadata.insert("base_url".into(), mock.url.clone().into());
    }
    let cfg = Config::parse(fx["request"]["config"].as_str().unwrap_or_default()).unwrap();
    let exec = DevinExecutor::with_client(default_client());
    let runs = fx["request"]["repeat"].as_u64().unwrap_or(1);
    let (mut down, mut retry_after) = (None, None);
    let mut usage = std::sync::Arc::new(crate::kimi_fixture::UsageLog::default());
    let mut capture = std::sync::Arc::new(crate::kimi_fixture::CaptureLog::default());
    for _ in 0..runs {
        usage = std::sync::Arc::new(crate::kimi_fixture::UsageLog::default());
        capture = std::sync::Arc::new(crate::kimi_fixture::CaptureLog::default());
        let mut req = request(&fx, "");
        req.usage = usage.sink().with_capture(capture.sink());
        bind_session(&mut req);
        let result = exec.execute(&cred, req, &cfg).await;
        retry_after = result.as_ref().err().and_then(|e| e.retry_after);
        down = Some(downstream(result).await);
    }
    let down = down.unwrap();
    crate::kimi_fixture::assert_usage_like_go(name, &fx, &usage, down.failure.as_ref());
    // devin_executor.go's two RecordAPIRequest sites (with Go's request and response log
    // bodies) rendered as Go's request log.
    crate::kimi_fixture::assert_capture_like_go(name, &fx, &capture, &mock.url, &["Sentry-Trace"]);
    // The stream's raw chunks: Go passes the header line, then each interactions event
    // with one trailing newline (the formatted log trims both).
    let chunks = capture.chunks();
    if let Some(at) = chunks
        .iter()
        .position(|c| c == b"=== INTERMEDIATE INTERACTIONS STREAM ===\n")
    {
        for event in chunks[at + 1..].iter().filter(|c| c.starts_with(b"{")) {
            assert!(
                event.ends_with(b"}\n"),
                "{name}: event chunk {:?}",
                String::from_utf8_lossy(event)
            );
        }
    } else {
        assert!(
            !fx["request"]["stream"].as_bool().unwrap_or(false) || chunks.len() <= 1,
            "{name}: stream chunks {chunks:?}"
        );
    }
    // Upstream requests.
    let go: Vec<Captured> = fx["upstream"]
        .as_array()
        .map(|u| u.iter().map(Captured::from_fixture).collect())
        .unwrap_or_default();
    let rust = mock.captured();
    assert_eq!(rust.len(), go.len(), "{name}: upstream request count");
    let custom_trace = fx["attributes"]["header:Sentry-Trace"].is_string();
    let trace = regex::Regex::new("^[0-9a-f]{32}-[0-9a-f]{16}-1$").unwrap();
    for (r, g) in rust.iter().zip(&go) {
        assert_eq!(r.method, g.method, "{name}");
        assert_eq!(r.target, g.target, "{name}");
        let names = |c: &Captured| c.headers.iter().map(|(n, _)| n.clone()).collect::<Vec<_>>();
        assert_eq!(names(r), names(g), "{name}: header order");
        for ((n, rv), (_, gv)) in r.headers.iter().zip(&g.headers) {
            match n.as_str() {
                "Host" => {}
                "Sentry-Trace" if !custom_trace => {
                    assert!(trace.is_match(rv), "{name}: sentry trace {rv}");
                }
                // The frame embeds runtime.GOOS, so its length matches Go's only on Linux.
                "Content-Length" => {
                    assert_eq!(rv, &r.body.len().to_string(), "{name}: header {n}");
                    if go_os_matches_fixture() {
                        assert_eq!(rv, gv, "{name}: header {n}");
                    }
                }
                _ => assert_eq!(rv, gv, "{name}: header {n}"),
            }
        }
        let (mut rb, mut gb) = (r.body.clone(), g.body.clone());
        if go_os_matches_fixture() {
            for body in [&mut rb, &mut gb] {
                assert_eq!(&body[..1], &[0], "{name}: data frame");
                mask_proto(&mut body[5..], &[3, 1]);
            }
            assert_eq!(b64(&rb), b64(&gb), "{name}: GetChatMessage frame");
        }
    }
    // Downstream.
    let go = &fx["downstream"];
    if let Some(status) = go["err_status"].as_i64()
        && go["stream_err"].is_null()
    {
        let status = if status == -1 { 500 } else { status as u16 };
        assert_eq!(down.err_status, Some(status), "{name}: error status");
        assert_eq!(down.err_body.as_deref(), go["err_body"].as_str(), "{name}: error body");
        let want = fx["extra"]["retry_after_secs"].as_f64().map(Duration::from_secs_f64);
        assert_eq!(retry_after, want, "{name}: retry hint");
        return;
    }
    if let Some(body) = go["body"].as_str().filter(|b| !b.is_empty()) {
        assert_eq!(
            down.body.as_deref().map(mask_output),
            Some(mask_output(body)),
            "{name}: body"
        );
        return;
    }
    let source = fx["request"]["source"].as_str().unwrap();
    let client = crate::kimi_fixture::format(source);
    let go_frames: Vec<String> = if source == "openai-response" {
        go["frames"]
            .as_array()
            .unwrap()
            .iter()
            .map(|c| mask_output(c.as_str().unwrap()))
            .collect()
    } else {
        go["chunks"]
            .as_array()
            .map(|c| {
                c.iter()
                    .filter_map(|c| cpa_translate::stream::frame(client, c.as_str().unwrap().as_bytes()))
                    .map(|f| mask_output(&String::from_utf8(f).unwrap()))
                    .collect()
            })
            .unwrap_or_default()
    };
    let rust_frames: Vec<String> = down.chunks.iter().map(|c| mask_output(c)).collect();
    assert_eq!(rust_frames, go_frames, "{name}: stream frames");
    match go["stream_err"].as_str() {
        Some(message) => {
            let status = go["err_status"].as_i64().unwrap();
            let status = if status == -1 { 500 } else { status as u16 };
            assert_eq!(down.err_status, Some(status), "{name}: stream error status");
            assert_eq!(down.err_body.as_deref(), Some(message), "{name}: stream error");
        }
        None => assert!(down.err_status.is_none(), "{name}: unexpected stream error"),
    }
}

#[tokio::test]
async fn executor_matches_go_fixtures() {
    for name in EXECUTOR_FIXTURES {
        check_executor_fixture(name).await;
    }
}

#[tokio::test]
async fn turn_index_advances_per_session_like_go() {
    // Two requests on one session: the second carries turn index 1 (field 15.2).
    check_executor_fixture("turn-index-repeat").await;
}

/// An observer that drops response events, as the server's request log does while
/// `request-log` is off.
struct ResponsesOff;

impl cpa_core::exec::CaptureObserver for ResponsesOff {
    fn record(&self, _: cpa_core::exec::CaptureEvent<'_>) {}
    fn logs_responses(&self) -> bool {
        false
    }
}

/// The stream summary copies thinking, content and the signature only while response
/// logging is on; otherwise a long stream would be held in memory for nothing.
#[tokio::test]
async fn stream_summary_buffers_only_while_responses_are_logged() {
    use cpa_core::exec::CaptureSink;
    let fx = fixture("devin", "chat-stream");
    let body = STANDARD
        .decode(fx["responses"][0]["body_b64"].as_str().unwrap())
        .unwrap();
    let logged = std::sync::Arc::new(crate::kimi_fixture::CaptureLog::default());
    for (sink, buffered) in [
        (CaptureSink::default(), false),
        (CaptureSink::new(std::sync::Arc::new(ResponsesOff)), false),
        (logged.sink(), true),
    ] {
        let reader = FrameReader::new(futures_util::stream::iter([Ok(Bytes::from(body.clone()))]).boxed());
        let claude = ClaudeInputTokens::new(Format::OpenAI, Format::Interactions, Format::Interactions, Bytes::new());
        let mut stream = DevinStream::new(
            "swe-2".into(),
            Format::Interactions,
            None,
            claude,
            reader,
            Default::default(),
            sink,
        );
        let (mut frames, mut held) = (0, 0);
        while stream.reader.is_some() {
            stream.step().await;
            let s = &stream.summary;
            frames = frames.max(s.frames);
            held = held.max(s.thinking.len() + s.content.len() + s.signature.len());
        }
        assert!(frames > 1, "frames read: {frames}");
        assert_eq!(held > 0, buffered, "summary buffered: {held} bytes");
    }
    assert!(
        logged
            .chunks()
            .iter()
            .any(|c| c.starts_with(b"\n=== DEVIN UPSTREAM RESPONSE SUMMARY ===")),
        "summary logged"
    );
}

#[tokio::test]
async fn refresh_matches_go_fixtures() {
    for name in [
        "refresh-user-status",
        "refresh-sparse-status",
        "refresh-error",
        "refresh-no-token",
    ] {
        let fx = fixture("devin", name);
        let mock = Mock::start(&fx["responses"]).await;
        let mut cred = crate::kimi_fixture::credential("devin", &fx, Some(("base_url", mock.url.clone())));
        cred.id = "devin-fixture.json".into();
        let exec = DevinExecutor::with_client(default_client());
        if let Some(before) = fx["extra"]["quota_before"].as_object() {
            exec.observe_quota(
                &cred.id,
                before
                    .iter()
                    .map(|(k, v)| (k.clone(), v.as_str().unwrap().to_owned()))
                    .collect(),
            );
        }
        let result = exec.prepare(&cred, &Config::parse("").unwrap()).await;
        let go: Vec<Captured> = fx["upstream"]
            .as_array()
            .map(|u| u.iter().map(Captured::from_fixture).collect())
            .unwrap_or_default();
        let rust = mock.captured();
        assert_eq!(rust.len(), go.len(), "{name}: upstream requests");
        let seeded = cred.str("device_seed").is_some_and(|s| !s.is_empty());
        for (r, g) in rust.iter().zip(&go) {
            let (mut r, mut g) = (r.clone(), g.clone());
            if !seeded {
                // Without a device seed the fingerprint is random.
                mask_proto(&mut r.body, &[1, 31]);
                mask_proto(&mut g.body, &[1, 31]);
            }
            if go_os_matches_fixture() {
                crate::kimi_fixture::assert_same_request(name, &g, &r, &["Host"]);
            }
        }
        match fx["downstream"]["err_body"].as_str() {
            Some(message) => {
                let rust = String::from_utf8(result.unwrap_err().body.to_vec()).unwrap();
                if name == "refresh-error" {
                    // Go appends the seat-management body; it is withheld.
                    crate::kimi_fixture::assert_go_message_without_body(name, &rust, message);
                } else {
                    assert_eq!(rust, message, "{name}");
                }
                continue;
            }
            None => {
                let patch = result.unwrap();
                let mut after = cred.metadata.clone();
                patch.apply(&mut after);
                after.remove("base_url");
                assert_eq!(Value::Object(after), fx["extra"]["metadata_after"], "{name}: metadata");
            }
        }
        let signals = exec.quota(&cred.id).map(|s| s.signals).unwrap_or_default();
        let want = fx["extra"]["quota_signals"].as_object().cloned().unwrap_or_default();
        assert_eq!(json!(signals), Value::Object(want), "{name}: quota signals");
    }
}

#[tokio::test]
async fn auth_record_matches_go_fixtures() {
    for name in [
        "record-profile",
        "record-status-only",
        "record-nothing",
        "record-unsafe-name",
    ] {
        let fx = fixture("devin", name);
        let mock = Mock::start(&fx["responses"]).await;
        let auth = DevinAuth::new(default_client())
            .with_api_base(&mock.url)
            .with_server_base(&mock.url)
            .with_app_base(&mock.url);
        let record = auth
            .create_auth_record(fx["request"]["token"].as_str().unwrap())
            .await
            .unwrap();
        let go = &fx["extra"];
        assert_eq!(record.file_name, go["file_name"].as_str().unwrap(), "{name}");
        assert_eq!(record.label, go["label"].as_str().unwrap(), "{name}");
        assert_eq!(json!(record.attributes), go["attributes"], "{name}");
        assert_eq!(Value::Object(record.metadata.clone()), go["metadata"], "{name}");
        assert_eq!(json!(record.quota_signals), go["quota_signals"], "{name}");
        let go_requests: Vec<Captured> = fx["upstream"]
            .as_array()
            .unwrap()
            .iter()
            .map(Captured::from_fixture)
            .collect();
        let rust = mock.captured();
        assert_eq!(rust.len(), go_requests.len(), "{name}");
        for (r, g) in rust.iter().zip(&go_requests) {
            let (mut r, mut g) = (r.clone(), g.clone());
            if r.target.ends_with("GetUserStatus") {
                // The login fingerprint is random (no device seed).
                mask_proto(&mut r.body, &[1, 31]);
                mask_proto(&mut g.body, &[1, 31]);
                if !go_os_matches_fixture() {
                    continue;
                }
            }
            crate::kimi_fixture::assert_same_request(name, &g, &r, &["Host"]);
        }
    }
}

#[tokio::test]
async fn code_exchange_matches_go_fixtures() {
    for name in ["exchange-ok", "exchange-error", "exchange-no-token"] {
        let fx = fixture("devin", name);
        let mock = Mock::start(&fx["responses"]).await;
        let auth = DevinAuth::new(default_client()).with_api_base(&mock.url);
        let result = auth
            .exchange_code(fx["request"]["exchange"].as_str().unwrap(), " verifier-1 ")
            .await;
        let go = &fx["downstream"];
        match go["err_body"].as_str() {
            // Go appends the token endpoint's body, which can carry the session token; it
            // is withheld because login logs this error.
            Some(message) => crate::kimi_fixture::assert_go_message_without_body(name, &result.unwrap_err(), message),
            None => assert_eq!(result.unwrap(), go["body"].as_str().unwrap(), "{name}"),
        }
        let g = Captured::from_fixture(&fx["upstream"][0]);
        crate::kimi_fixture::assert_same_request(name, &g, &mock.captured()[0], &["Host"]);
    }
}

#[tokio::test]
async fn login_save_merges_like_go_manager() {
    let fx = fixture("devin", "login-save");
    let mock = Mock::start(&fx["responses"]).await;
    let auth = DevinAuth::new(default_client())
        .with_api_base(&mock.url)
        .with_server_base(&mock.url);
    let record = auth
        .create_auth_record(fx["request"]["token"].as_str().unwrap())
        .await
        .unwrap();
    let dir = std::env::temp_dir().join(format!("devin-login-save-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    let file_name = fx["extra"]["file_name"].as_str().unwrap();
    std::fs::write(dir.join(file_name), fx["extra"]["stale"].as_str().unwrap()).unwrap();
    let path = save_record(&dir, &record).unwrap();
    assert_eq!(path.file_name().unwrap().to_str().unwrap(), file_name);
    assert_eq!(
        std::fs::read_to_string(&path).unwrap(),
        fx["extra"]["file"].as_str().unwrap()
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn devin_sessions_are_uuids() {
    // Go normalizeDevinUUID: v5 of a non-UUID session (same namespace and name as Go).
    let v5 = normalize_uuid("msg:abc");
    assert_eq!(v5, normalize_uuid(" msg:abc "));
    assert!(go_uuid_parses(&v5));
    assert!(go_uuid_parses(&normalize_uuid("")), "random v4");
}

#[test]
fn sensitive_words_drop_matching_lines_then_obfuscate() {
    let m = SensitiveWords::new(&["secret".into()]).unwrap();
    assert!(m.matches(b"a SECRET b"));
    assert_eq!(m.obfuscate(b"Secret"), "S\u{200b}ecret".as_bytes());
    // A line containing a word is dropped; nothing is left to obfuscate.
    assert_eq!(sanitize_system_prompt(b"keep\nhas secret\nend", Some(&m)), b"keep\nend");
}
