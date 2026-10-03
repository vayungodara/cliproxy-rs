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

/// The model info Go's conductor bound for the scenario (the fixture's `resolved`).
pub(crate) fn resolved_model(s: &Value) -> Option<cpa_core::exec::ResolvedModel> {
    let record = s.get("resolved").filter(|r| !r.is_null())?;
    let mut raw = record["info"].as_object().unwrap().clone();
    for flag in ["is_compat", "user_defined", "support_configuration_update"] {
        if record[flag].as_bool() == Some(true) {
            raw.insert(flag.into(), Value::Bool(true));
        }
    }
    Some(cpa_core::exec::ResolvedModel {
        info: cpa_core::registry::ModelInfo::from_raw(raw).unwrap(),
        source: cpa_core::exec::ResolvedSource::ApiKey,
    })
}

/// What an executor reported to its usage sink: the reasoning effort of the last
/// translated request (Go `SetTranslatedReasoningEffort`) and how many response reports
/// arrived. Token and response-model parity is checked through the router against Go's
/// published records (cpa-server tests/gemini_routes.rs `usage_records_match_go`).
#[derive(Default)]
pub(crate) struct UsageReports {
    effort: Mutex<Option<String>>,
    responses: Mutex<usize>,
}

impl UsageReports {
    pub(crate) fn sink(self: &Arc<Self>) -> cpa_core::exec::UsageSink {
        cpa_core::exec::UsageSink::new(self.clone())
    }

    /// Checks the reports against the record Go's reporter published (the fixture's
    /// `usage`): Go's record keeps an empty effort unless the executor set one.
    pub(crate) fn check(&self, s: &Value) {
        let name = s["name"].as_str().unwrap();
        let effort = self.effort.lock().unwrap().clone().unwrap_or_default();
        match s.get("usage").filter(|u| !u.is_null()) {
            Some(want) => assert_eq!(
                effort,
                want["reasoning_effort"].as_str().unwrap(),
                "{name}: reasoning effort"
            ),
            // No record: Go failed before its reporter existed, so nothing was sent.
            None => assert_eq!(*self.responses.lock().unwrap(), 0, "{name}: no reporter in Go"),
        }
    }
}

impl cpa_core::exec::UsageObserver for UsageReports {
    fn response_body(&self, _: Format, _: &[u8]) {
        *self.responses.lock().unwrap() += 1;
    }

    fn response_line(&self, _: Format, _: &[u8]) {
        *self.responses.lock().unwrap() += 1;
    }

    fn request(&self, format: Format, payload: &[u8]) {
        let effort = cpa_common::thinking::extract_translated_reasoning_effort(payload, format.as_str());
        *self.effort.lock().unwrap() = Some(effort);
    }
}

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
        resolved_model: resolved_model(s),
        usage: Default::default(),
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
pub(crate) fn client_bytes(client: Format, alt: bool, chunks: &[Value]) -> Vec<u8> {
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

#[tokio::test]
async fn go_reference_scenarios() {
    let scenarios = fixture()["scenarios"].as_array().unwrap();
    assert!(scenarios.len() >= 60, "fixture lost scenarios");
    let executor = GeminiExecutor::default();
    let mut ran = 0;
    let mut skipped = Vec::new();
    let ids = regex::Regex::new(r"interaction_[0-9]{16,20}").unwrap();
    let stamps = regex::Regex::new(r#"\"(created|updated)\":\"[0-9]{4}-[0-9]{2}-[0-9]{2}T[0-9:]{8}Z\""#).unwrap();
    // Interactions -> OpenAI chat stamps `created` with time.Now().Unix() (Go
    // openai_interactions_response.go); only 10-digit Unix times count as clock values.
    let unix = regex::Regex::new(r#"\"created\":1[0-9]{9}([,}])"#).unwrap();
    for s in scenarios {
        let name = s["name"].as_str().unwrap();
        let missing = missing(s);
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
        let mut req = request(s);
        let reports = Arc::new(UsageReports::default());
        req.usage = reports.sink();
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
            let text = stamps.replace_all(&text, r#""$1":"<time>""#);
            unix.replace_all(&text, r#""created":<unix>$1"#).into_owned()
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
        reports.check(s);
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
        translator: Box::new(Pending(None)),
        client: Format::OpenAIResponse,
        raw: false,
        claude: ClaudeInputTokens::new(
            Format::OpenAIResponse,
            Format::Gemini,
            Format::OpenAIResponse,
            Bytes::new(),
        ),
        usage: Default::default(),
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

/// helps.StopApplyPatchStream / EndApplyPatchStream: a failed tool input ends the stream
/// with the sanitized 502 right after that event's frames, and at EOF the translator's
/// finalization runs before the synthetic `[DONE]` (which a stopped stream never sees).
#[tokio::test]
async fn apply_patch_hooks_follow_go_order() {
    #[derive(Default)]
    struct Hooks {
        seen: Arc<Mutex<Vec<String>>>,
        fail_on: Option<&'static [u8]>,
        failed: bool,
        finalize_fails: bool,
    }
    impl StreamTranslator for Hooks {
        fn event(&mut self, event: &[u8]) -> Result<Vec<Bytes>, cpa_translate::Error> {
            self.seen
                .lock()
                .unwrap()
                .push(String::from_utf8_lossy(event).into_owned());
            if self.fail_on == Some(event) {
                self.failed = true;
                return Ok(vec![Bytes::from_static(b"event: response.failed\ndata: {}\n\n")]);
            }
            Ok(vec![Bytes::from(format!(
                "data: {}\n\n",
                String::from_utf8_lossy(event)
            ))])
        }
        fn finish(&mut self) -> Result<Vec<Bytes>, cpa_translate::Error> {
            self.seen.lock().unwrap().push("finish".into());
            Ok(Vec::new())
        }
        fn tool_input_failed(&self) -> bool {
            self.failed
        }
        fn finalize_tool_input(&mut self) -> Vec<Bytes> {
            self.seen.lock().unwrap().push("finalize".into());
            if self.finalize_fails {
                self.failed = true;
                return vec![Bytes::from_static(b"event: response.failed\ndata: {\"eof\":1}\n\n")];
            }
            Vec::new()
        }
    }
    let run = |hooks: Hooks| async move {
        let output = Output {
            translator: Box::new(hooks),
            client: Format::OpenAI,
            raw: false,
            claude: ClaudeInputTokens::new(Format::OpenAI, Format::Gemini, Format::OpenAI, Bytes::new()),
            usage: Default::default(),
        };
        let lines = futures_util::stream::iter([
            Ok(Bytes::from_static(br#"data: {"a":1}"#)),
            Ok(Bytes::from_static(br#"data: {"b":2}"#)),
        ])
        .boxed();
        let items: Vec<Result<Bytes, ExecError>> = gemini_lines(lines, output).collect().await;
        items
            .into_iter()
            .map(|i| match i {
                Ok(b) => String::from_utf8(b.to_vec()).unwrap(),
                Err(e) => format!("ERR {} {}", e.status, String::from_utf8_lossy(&e.body)),
            })
            .collect::<Vec<_>>()
    };
    let stop = format!("ERR 502 {EMPTY_TRANSLATION}");

    let seen = Arc::new(Mutex::new(Vec::new()));
    let out = run(Hooks {
        seen: seen.clone(),
        ..Hooks::default()
    })
    .await;
    assert_eq!(out, ["data: {\"a\":1}\n\n", "data: {\"b\":2}\n\n", "data: [DONE]\n\n"]);
    assert_eq!(
        *seen.lock().unwrap(),
        [r#"{"a":1}"#, r#"{"b":2}"#, "finalize", "[DONE]", "finish"]
    );

    let seen = Arc::new(Mutex::new(Vec::new()));
    let out = run(Hooks {
        seen: seen.clone(),
        fail_on: Some(br#"{"a":1}"#),
        ..Hooks::default()
    })
    .await;
    assert_eq!(out, ["event: response.failed\ndata: {}\n\n".to_owned(), stop.clone()]);
    assert_eq!(*seen.lock().unwrap(), [r#"{"a":1}"#]);

    let seen = Arc::new(Mutex::new(Vec::new()));
    let out = run(Hooks {
        seen: seen.clone(),
        finalize_fails: true,
        ..Hooks::default()
    })
    .await;
    assert_eq!(
        out,
        [
            "data: {\"a\":1}\n\n".to_owned(),
            "data: {\"b\":2}\n\n".to_owned(),
            "event: response.failed\ndata: {\"eof\":1}\n\n".to_owned(),
            stop
        ]
    );
    assert_eq!(*seen.lock().unwrap(), [r#"{"a":1}"#, r#"{"b":2}"#, "finalize"]);
}
