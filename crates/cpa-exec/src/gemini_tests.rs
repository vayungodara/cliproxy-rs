//! Differential tests against `tests/fixtures/gemini_go.json`, produced by the real Go
//! Gemini and Interactions executors (tests/reference/gemini). Each scenario replays the
//! scripted upstream answer to the Rust executor and compares the raw upstream request,
//! the output, the client stream bytes and errors. Expected values come only from Go.

use std::sync::{Arc, Mutex};

use cpa_core::exec::Caller;
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};

use super::*;
use crate::{gemini_payload as payload, gemini_stream as sse};

const FIXTURE: &str = include_str!("../tests/fixtures/gemini_go.json");

fn fixture() -> &'static Value {
    static PARSED: std::sync::OnceLock<Value> = std::sync::OnceLock::new();
    PARSED.get_or_init(|| serde_json::from_str(FIXTURE).unwrap())
}

/// One-shot raw HTTP/1.1 capture server answering with the scripted response.
struct Mock {
    addr: String,
    raw: Arc<Mutex<Option<Vec<u8>>>>,
}

impl Mock {
    async fn start(up: &Value) -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        let raw: Arc<Mutex<Option<Vec<u8>>>> = Arc::default();
        let sink = raw.clone();
        let status = up["status"].as_u64().unwrap() as u16;
        let headers: Vec<(String, String)> = up["headers"]
            .as_array()
            .map(|hs| {
                hs.iter()
                    .map(|h| (h[0].as_str().unwrap().to_owned(), h[1].as_str().unwrap().to_owned()))
                    .collect()
            })
            .unwrap_or_default();
        let mut body = up["body"].as_str().unwrap().as_bytes().to_vec();
        if up["gzip"].as_bool() == Some(true) {
            use async_compression::tokio::bufread::GzipEncoder;
            let mut encoder = GzipEncoder::new(std::io::Cursor::new(body));
            let mut out = Vec::new();
            encoder.read_to_end(&mut out).await.unwrap();
            body = out;
        }
        tokio::spawn(async move {
            let Ok((socket, _)) = listener.accept().await else {
                return;
            };
            let (read, mut write) = socket.into_split();
            let mut reader = BufReader::new(read);
            let mut captured = Vec::new();
            let mut length = 0;
            loop {
                let mut line = Vec::new();
                if reader.read_until(b'\n', &mut line).await.unwrap_or(0) == 0 {
                    break;
                }
                captured.extend_from_slice(&line);
                let text = String::from_utf8_lossy(&line).to_ascii_lowercase();
                if let Some(v) = text.strip_prefix("content-length:") {
                    length = v.trim().parse().unwrap_or(0);
                }
                if line == b"\r\n" {
                    break;
                }
            }
            let mut payload = vec![0; length];
            reader.read_exact(&mut payload).await.unwrap();
            captured.extend_from_slice(&payload);
            *sink.lock().unwrap() = Some(captured);
            let reason = http::StatusCode::from_u16(status)
                .ok()
                .and_then(|s| s.canonical_reason())
                .unwrap_or("");
            let mut out = format!("HTTP/1.1 {status} {reason}\r\n").into_bytes();
            for (n, v) in &headers {
                out.extend_from_slice(format!("{n}: {v}\r\n").as_bytes());
            }
            out.extend_from_slice(format!("Content-Length: {}\r\n", body.len()).as_bytes());
            out.extend_from_slice(b"Connection: close\r\n\r\n");
            out.extend_from_slice(&body);
            let _ = write.write_all(&out).await;
            let _ = write.shutdown().await;
        });
        Self { addr, raw }
    }

    fn request(&self) -> Option<String> {
        let raw = self.raw.lock().unwrap().clone()?;
        Some(String::from_utf8(raw).unwrap().replace(&self.addr, "UPSTREAM"))
    }
}

fn credential(s: &Value, cfg: &Config, addr: &str) -> Credential {
    let provider = s["provider"].as_str().unwrap();
    let index = s["config_auth"].as_i64().unwrap();
    if index >= 0 {
        let matching: Vec<Credential> = cpa_core::config::credentials::from_config(cfg)
            .into_iter()
            .filter(|c| c.provider == provider)
            .collect();
        return matching[index as usize].clone();
    }
    let mut metadata = serde_json::Map::new();
    metadata.insert("type".into(), provider.into());
    let mut c = Credential::from_file(
        std::path::Path::new("/fixture"),
        std::path::Path::new("/fixture/gemini.json"),
        metadata,
    )
    .unwrap();
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
        requested_model: s["requested_model"].as_str().unwrap_or(&model).to_owned(),
        model,
        original_body: original,
        body,
        stream: op == "stream",
        alt: s["alt"].as_str().map(str::to_owned),
        session: s["session"].as_str().map(str::to_owned),
        execution_session: None,
        derived_session: None,
        request_path: String::new(),
        headers,
        caller: Caller {
            principal: "fake-client-key".into(),
            source: "authorization",
        },
    }
}

/// The bytes Go's route handler writes for the executor's stream chunks, written out
/// from the handlers rather than taken from the Rust framer. Empty chunks never reach a
/// handler (handlers_stream.go). The Responses route's terminal tracking and the Gemini
/// route's keep-alives belong to the server, so this stops at each chunk's framing.
fn client_bytes(client: Format, alt: bool, chunks: &[Value]) -> Vec<u8> {
    let mut out = Vec::new();
    for chunk in chunks {
        let chunk = chunk.as_str().unwrap().as_bytes();
        if chunk.is_empty() {
            continue;
        }
        match client {
            // gemini_handlers.go: `data: ` + chunk + `\n\n`, or the chunk with an alt.
            Format::Gemini if alt => out.extend_from_slice(chunk),
            // openai_handlers.go: fmt.Fprintf("data: %s\n\n").
            Format::Gemini | Format::OpenAI => {
                out.extend_from_slice(b"data: ");
                out.extend_from_slice(chunk);
                out.extend_from_slice(b"\n\n");
            }
            // interactions_handlers.go WriteChunk.
            Format::Interactions => {
                let trimmed = cpa_common::gostr::trim_space(chunk);
                if !(trimmed.starts_with(b"event:") || trimmed.starts_with(b"data:")) {
                    out.extend_from_slice(b"data: ");
                }
                out.extend_from_slice(chunk);
                if !chunk.ends_with(b"\n\n") {
                    out.extend_from_slice(b"\n\n");
                }
            }
            // responsesSSEFramer for complete events: the event and its blank line.
            Format::OpenAIResponse => {
                out.extend_from_slice(chunk);
                if !chunk.ends_with(b"\n\n") {
                    out.extend_from_slice(b"\n\n");
                }
            }
            // code_handlers.go: the chunk as is.
            _ => out.extend_from_slice(chunk),
        }
    }
    out
}

/// Translator registrations a scenario needs that Rust does not have yet.
fn missing(s: &Value) -> Vec<String> {
    let parse = |entry: &str| {
        let (client, upstream) = entry.split_once("->").unwrap();
        (Format::parse(client).unwrap(), Format::parse(upstream).unwrap())
    };
    s["needs"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .filter(|need| match need.split_once(':') {
            Some(("pair", pair)) => {
                let (c, u) = parse(pair);
                cpa_translate::pair(c, u).is_none()
            }
            Some(("token_count", pair)) => {
                let (c, u) = parse(pair);
                cpa_translate::token_count(c, u).is_none()
            }
            _ => panic!("unknown need {need}"),
        })
        .map(str::to_owned)
        .collect()
}

/// Non-Interactions clients on a native Interactions upstream. They became runnable when
/// the Interactions pairs landed (translate increments 8-11) and fail until the executor
/// feeds Interactions SSE through `pair.stream` the way `stream::Framed` expects (raw
/// bytes per read, `finish()` at EOF).
// ponytail: integrator gate; the Google thread removes each name as it passes.
const AWAITING_INTERACTIONS_FRAMING: &[&str] = &[
    "int_gemini_client_stream",
    "int_openai_client",
    "int_claude_client_stream",
    "int_responses_client_stream",
];

#[tokio::test]
async fn go_reference_scenarios() {
    let scenarios = fixture()["scenarios"].as_array().unwrap();
    assert!(scenarios.len() >= 60, "fixture lost scenarios");
    let executor = GeminiExecutor::default();
    let mut ran = 0;
    let mut skipped = Vec::new();
    let ids = regex::Regex::new(r"interaction_[0-9]{16,20}").unwrap();
    let stamps = regex::Regex::new(r#"\"(created|updated)\":\"[0-9]{4}-[0-9]{2}-[0-9]{2}T[0-9:]{8}Z\""#).unwrap();
    for s in scenarios {
        let name = s["name"].as_str().unwrap();
        let missing = missing(s);
        if AWAITING_INTERACTIONS_FRAMING.contains(&name) {
            skipped.push(format!("{name} (Interactions-upstream framing)"));
            continue;
        }
        if !missing.is_empty() {
            skipped.push(format!("{name} ({})", missing.join(", ")));
            continue;
        }
        let mock = match s.get("upstream").filter(|u| !u.is_null()) {
            Some(up) => Some(Mock::start(up).await),
            None => None,
        };
        let addr = mock.as_ref().map_or("127.0.0.1:9".to_owned(), |m| m.addr.clone());
        let cfg = Config::parse(&s["config"].as_str().unwrap_or_default().replace("UPSTREAM", &addr)).unwrap();
        let cred = credential(s, &cfg, &addr);
        let req = request(s);
        let (client, alt) = (req.response_format, req.alt.as_deref().is_some_and(|a| !a.is_empty()));
        let mut output = None;
        let mut streamed = Vec::new();
        let mut error = None;
        match executor.execute(&cred, req, &cfg).await {
            Err(e) => error = Some((e.status, String::from_utf8_lossy(&e.body).into_owned())),
            Ok(response) => match response.body {
                ResponseBody::Buffered(bytes) => output = Some(String::from_utf8(bytes.to_vec()).unwrap()),
                ResponseBody::Stream(mut stream) => {
                    while let Some(item) = stream.next().await {
                        match item {
                            Ok(bytes) => streamed.extend_from_slice(&bytes),
                            Err(e) => error = Some((e.status, String::from_utf8_lossy(&e.body).into_owned())),
                        }
                    }
                }
            },
        }
        let want_error = s["error"].as_object().map(|e| {
            (
                e["status"].as_u64().unwrap() as u16,
                e["message"].as_str().unwrap().to_owned(),
            )
        });
        assert_eq!(error, want_error, "{name}: error");
        // Go mints Interactions IDs from the wall clock (`interaction_<UnixNano>`); only
        // their shape can match a recorded fixture.
        // Their `created`/`updated` stamps are wall-clock RFC 3339 times as well.
        let norm = |text: &str| {
            let text = ids.replace_all(text, "interaction_<id>");
            stamps.replace_all(&text, r#""$1":"<time>""#).into_owned()
        };
        assert_eq!(
            output.as_deref().map(norm),
            s["output"].as_str().map(norm),
            "{name}: output"
        );
        let want_stream = client_bytes(client, alt, s["chunks"].as_array().map_or(&[][..], Vec::as_slice));
        assert_eq!(
            norm(&String::from_utf8_lossy(&streamed)),
            norm(&String::from_utf8_lossy(&want_stream)),
            "{name}: stream"
        );
        let request = mock.as_ref().and_then(Mock::request);
        assert_eq!(request.as_deref(), s["request"].as_str(), "{name}: upstream request");
        ran += 1;
    }
    eprintln!("gemini scenarios: {ran} ran, {} wait for translators:", skipped.len());
    for name in &skipped {
        eprintln!("  {name}");
    }
    assert!(ran >= 45, "too few scenarios ran: {ran}");
}

fn pairs(name: &str) -> Vec<(&'static str, &'static str)> {
    fixture()["vectors"][name]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| (p["in"].as_str().unwrap(), p["out"].as_str().unwrap()))
        .collect()
}

/// `helps.FilterSSEUsageMetadata`, in Go's call order (trace IDs are remembered).
#[test]
fn filter_sse_usage_matches_go() {
    let vectors = pairs("filter_sse_usage");
    assert!(vectors.len() > 20);
    for (input, want) in vectors {
        let got = sse::filter_sse_usage_metadata(input.as_bytes());
        assert_eq!(String::from_utf8_lossy(&got), want, "input {input:?}");
    }
}

#[test]
fn json_payload_matches_go() {
    for (input, want) in pairs("json_payload") {
        let got = sse::json_payload(input.as_bytes()).unwrap_or_default();
        assert_eq!(String::from_utf8_lossy(got), want, "input {input:?}");
    }
}

#[test]
fn boundary_user_turns_match_go() {
    let leading = pairs("leading_user_content");
    for (i, (input, want)) in leading.iter().enumerate() {
        let path = if i == leading.len() - 1 {
            "request.contents"
        } else {
            "contents"
        };
        let got = payload::ensure_leading_user_content(input.as_bytes().to_vec(), path);
        assert_eq!(String::from_utf8_lossy(&got), *want, "leading {input:?}");
    }
    for (input, want) in pairs("trailing_user") {
        let got = payload::ensure_trailing_user_content(input.as_bytes().to_vec(), "contents");
        assert_eq!(String::from_utf8_lossy(&got), want, "trailing {input:?}");
    }
}

/// `TranslateStreamWithClaudeInputTokens` with a transform-less upstream: chunks pass
/// through and the first `message_start` without input tokens gets the estimate.
#[test]
fn claude_input_tokens_match_go() {
    let cases = fixture()["vectors"]["claude_input_tokens"].as_array().unwrap();
    assert!(cases.len() >= 7);
    for case in cases {
        let original = case["original"].as_str().unwrap();
        let mut state = ClaudeInputTokens::new(
            Format::Claude,
            Format::Gemini,
            Format::Claude,
            Bytes::from(original.to_owned()),
        );
        let mut got = Vec::new();
        for chunk in case["chunks"].as_array().unwrap() {
            let mut out = vec![Bytes::from(chunk.as_str().unwrap().to_owned())];
            state.apply(&mut out);
            got.extend(out.into_iter().map(|b| String::from_utf8(b.to_vec()).unwrap()));
        }
        let want: Vec<String> = case["out"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap().to_owned())
            .collect();
        assert_eq!(got, want, "original {original}");
    }
}

/// The resolved configured model decides thinking validation (Go binds it in the
/// conductor): a configured `levels` list rejects levels the static model would map.
#[test]
fn resolved_model_follows_config_routes() {
    let cfg = Config::parse(
        r#"
api-keys:
  gemini:
    - base-url: http://example.invalid
      prefix: team
      models:
        - name: gemini-2.5-pro
          alias: pro-levels
          is-compat: true
          thinking:
            levels: [LOW, High, none, high]
        - name: gemini-2.5-flash(1024)
          alias: suffixed
      keys:
        - api-key: AIza-fake
"#,
    )
    .unwrap();
    let cred = cpa_core::config::credentials::from_config(&cfg)
        .into_iter()
        .find(|c| c.provider == "gemini")
        .unwrap();
    let resolved = payload::resolved_model(
        &cred,
        &cfg,
        "gemini",
        "gemini",
        "team/pro-levels(high)",
        "gemini-2.5-pro(high)",
    )
    .unwrap();
    assert!(resolved.is_compat);
    assert_eq!(resolved.caps.id, "gemini-2.5-pro");
    assert_eq!(resolved.caps.kind, "gemini");
    assert!(!resolved.caps.user_defined);
    let thinking = resolved.caps.thinking.unwrap();
    assert_eq!(thinking.levels, ["low", "high", "none"]);
    assert!(thinking.zero_allowed);
    // Configured thinking replaces the static support, budget limits included.
    assert_eq!((thinking.min, thinking.max), (0, 0));
    // A configured suffix must match exactly; the base-name fallback is only for
    // suffix-free configured names.
    assert!(payload::resolved_model(&cred, &cfg, "gemini", "gemini", "suffixed", "gemini-2.5-flash").is_none());
    assert!(payload::resolved_model(&cred, &cfg, "gemini", "gemini", "suffixed", "gemini-2.5-flash(1024)").is_some());
    // Unconfigured routes resolve nothing (Go then looks the model up in the registry).
    assert!(payload::resolved_model(&cred, &cfg, "gemini", "gemini", "other", "gemini-2.5-pro").is_none());
}

#[test]
fn white_images_are_go_pngs() {
    use base64::Engine;
    for (ratio, (w, h)) in [
        ("16:9", (1344u32, 768u32)),
        ("21:9", (1536, 672)),
        ("7:3", (1024, 1024)),
    ] {
        let mut body =
            br#"{"contents":[{"parts":[{"text":"x"}]}],"generationConfig":{"imageConfig":{"aspectRatio":""}}}"#
                .to_vec();
        cpa_common::json::set_str(&mut body, "generationConfig.imageConfig.aspectRatio", ratio);
        let out = payload::fix_image_aspect_ratio("gemini-2.5-flash-image-preview", body);
        let data = cpa_common::json::get(&out, "contents.0.parts.1.inlineData.data")
            .str()
            .into_owned();
        let png = base64::engine::general_purpose::STANDARD.decode(data).unwrap();
        assert_eq!(&png[..8], b"\x89PNG\r\n\x1a\n");
        // IHDR width and height.
        assert_eq!(u32::from_be_bytes(png[16..20].try_into().unwrap()), w, "{ratio}");
        assert_eq!(u32::from_be_bytes(png[20..24].try_into().unwrap()), h, "{ratio}");
    }
}

/// Go schedules one unconditional delete ten minutes after every remember and never
/// cancels it (rememberStopWithoutUsage), so the first timer wins and an old timer can
/// delete a re-remembered trace.
#[test]
fn stop_memory_follows_go_timers() {
    use std::time::{Duration, Instant};
    let t0 = Instant::now();
    let at0 = |secs: f64| t0 + Duration::from_secs_f64(secs);
    let filter = |line: &str, now| String::from_utf8(sse::filter_sse_usage_metadata_at(line.as_bytes(), now)).unwrap();
    let stop = |t: &str| format!(r#"data: {{"traceId":"{t}","candidates":[{{"finishReason":"STOP"}}]}}"#);
    let usage = |t: &str| format!(r#"data: {{"traceId":"{t}","usageMetadata":{{"n":1}}}}"#);
    let renamed = |t: &str| format!(r#"data: {{"traceId":"{t}","cpaUsageMetadata":{{"n":1}}}}"#);

    // A second remember does not extend the first one's lifetime.
    filter(&stop("clock-a"), at0(0.0));
    filter(&stop("clock-a"), at0(599.0));
    assert_eq!(filter(&usage("clock-a"), at0(600.5)), renamed("clock-a"));

    // Within the window the usage chunk keeps its usage and consumes the entry.
    filter(&stop("clock-b"), at0(0.0));
    assert_eq!(filter(&usage("clock-b"), at0(100.0)), usage("clock-b"));
    assert_eq!(filter(&usage("clock-b"), at0(101.0)), renamed("clock-b"));
    // Re-remembered after consumption: the first timer still deletes it at 600s.
    filter(&stop("clock-b"), at0(200.0));
    assert_eq!(filter(&usage("clock-b"), at0(601.0)), renamed("clock-b"));

    // Without an older timer the same timeline keeps the usage.
    filter(&stop("clock-c"), at0(200.0));
    assert_eq!(filter(&usage("clock-c"), at0(601.0)), usage("clock-c"));
}

/// A configured model without `name` routes and resolves as its alias (Go normalizes
/// both before building the capability).
#[test]
fn alias_only_model_resolves_static_capabilities() {
    let cfg = Config::parse(
        "api-keys:\n  gemini:\n    - base-url: http://example.invalid\n      models:\n        - alias: gemini-2.5-flash\n      keys:\n        - api-key: AIza-fake\n",
    )
    .unwrap();
    let cred = cpa_core::config::credentials::from_config(&cfg)
        .into_iter()
        .find(|c| c.provider == "gemini")
        .unwrap();
    let resolved = payload::resolved_model(
        &cred,
        &cfg,
        "gemini",
        "gemini",
        "gemini-2.5-flash(1024)",
        "gemini-2.5-flash(1024)",
    )
    .unwrap();
    assert_eq!(resolved.caps.id, "gemini-2.5-flash");
    let thinking = resolved.caps.thinking.unwrap();
    assert_eq!((thinking.max, thinking.zero_allowed), (24576, true));
}

/// Configured budget thinking uses Go's YAML spellings (`zero-allowed`,
/// `dynamic-allowed`); both must reach the bound capabilities.
#[test]
fn configured_budget_thinking_keeps_yaml_flags() {
    let cfg = Config::parse(
        "api-keys:\n  gemini:\n    - base-url: http://example.invalid\n      models:\n        - name: gemini-2.5-pro\n          alias: budget\n          thinking:\n            min: 64\n            max: 2048\n            zero-allowed: true\n            dynamic-allowed: true\n      keys:\n        - api-key: AIza-fake\n",
    )
    .unwrap();
    let cred = cpa_core::config::credentials::from_config(&cfg)
        .into_iter()
        .find(|c| c.provider == "gemini")
        .unwrap();
    let resolved = payload::resolved_model(&cred, &cfg, "gemini", "gemini", "budget", "gemini-2.5-pro").unwrap();
    let thinking = resolved.caps.thinking.unwrap();
    assert_eq!(
        (
            thinking.min,
            thinking.max,
            thinking.zero_allowed,
            thinking.dynamic_allowed
        ),
        (64, 2048, true, true)
    );
}

/// Before a terminal error reaches a Responses client, the frame the translator is still
/// joining is written first (Go's responsesSSEFramer.Flush in WriteTerminalError).
#[tokio::test]
async fn pending_responses_frame_is_flushed_before_terminal_error() {
    struct Pending(Option<Bytes>);
    impl StreamTranslator for Pending {
        fn event(&mut self, event: &[u8]) -> Result<Vec<Bytes>, cpa_translate::Error> {
            if event == br#"{"fail":1}"# {
                return Err(cpa_translate::Error("bad tool input".into()));
            }
            self.0 = Some(Bytes::from(format!(
                "event: x\ndata: {}\n\n",
                String::from_utf8_lossy(event)
            )));
            Ok(Vec::new())
        }
        fn finish(&mut self) -> Result<Vec<Bytes>, cpa_translate::Error> {
            Ok(Vec::new())
        }
        fn flush_frames(&mut self) -> Vec<Bytes> {
            self.0.take().into_iter().collect()
        }
    }
    let output = Output {
        translator: Some(Box::new(Pending(None))),
        client: Format::OpenAIResponse,
        raw: false,
        claude: ClaudeInputTokens::new(
            Format::OpenAIResponse,
            Format::Gemini,
            Format::OpenAIResponse,
            Bytes::new(),
        ),
    };
    let lines = futures_util::stream::iter([
        Ok(Bytes::from_static(br#"data: {"a":1}"#)),
        Ok(Bytes::from_static(br#"data: {"fail":1}"#)),
        Ok(Bytes::from_static(br#"data: {"never":1}"#)),
    ])
    .boxed();
    let items: Vec<Result<Bytes, ExecError>> = gemini_lines(lines, output).collect().await;
    assert_eq!(items.len(), 2, "{items:?}");
    assert_eq!(
        items[0].as_ref().unwrap(),
        &Bytes::from_static(b"event: x\ndata: {\"a\":1}\n\n")
    );
    let error = items[1].as_ref().unwrap_err();
    assert_eq!((error.status, &error.body[..]), (502, EMPTY_TRANSLATION.as_bytes()));
}

/// Go `Auth.AuthKind` decides whether configured capabilities bind: a recognized
/// attribute, then a recognized metadata field, then a non-empty API key attribute.
#[test]
fn resolved_model_follows_go_auth_kind() {
    let cfg = Config::parse(
        "api-keys:\n  gemini:\n    - base-url: http://example.invalid\n      models:\n        - name: gemini-2.5-pro\n          alias: pro\n      keys:\n        - api-key: AIza-fake\n",
    )
    .unwrap();
    let base = cpa_core::config::credentials::from_config(&cfg)
        .into_iter()
        .find(|c| c.provider == "gemini")
        .unwrap();
    let resolve =
        |c: &Credential| payload::resolved_model(c, &cfg, "gemini", "gemini", "pro", "gemini-2.5-pro").is_some();
    assert!(resolve(&base));
    // An unknown attribute kind falls through to the API key.
    let mut unknown = base.clone();
    unknown.attributes.insert("auth_kind".into(), "weird".into());
    assert!(resolve(&unknown));
    // A metadata OAuth kind wins over the API key attribute.
    let mut oauth = unknown.clone();
    oauth.metadata.insert("auth_kind".into(), "oauth".into());
    assert!(!resolve(&oauth));
    // Without a kind, an empty API key is not an API-key credential.
    let mut empty = base.clone();
    empty.attributes.remove("auth_kind");
    empty.attributes.insert("api_key".into(), "  ".into());
    assert!(!resolve(&empty));
}
