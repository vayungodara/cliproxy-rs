//! `/v1/images/*`, `/v1/videos*` and `/openai/v1/videos*` against goldens recorded from
//! the unmodified Go server (CLIProxyAPI 6fecc6e), and `/v1/audio/speech` and `/v1/tts`
//! against goldens from v8.0.35, as are the passthrough error headers
//! (`passthrough_errors_go.json`), by tests/reference/media: the same config, client
//! requests and scripted upstream replies, compared on status, the kept response headers,
//! the response body and every raw upstream request.

use std::collections::{BTreeMap, VecDeque};
use std::sync::{Arc, Mutex};

use base64::Engine;
use cpa_core::config::Config;
use cpa_exec::Executors;
use cpa_exec::claude::ClaudeExecutor;
use cpa_server::router;
use regex::Regex;
use serde::Deserialize;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};

#[derive(Deserialize)]
struct Fixture {
    config: String,
    scenarios: Vec<Scenario>,
}

#[derive(Deserialize, Clone)]
struct Upstream {
    #[serde(default)]
    delay_ms: u64,
    status: u16,
    #[serde(default)]
    headers: Vec<(String, String)>,
    body: String,
}

#[derive(Deserialize)]
struct Scenario {
    name: String,
    method: String,
    path: String,
    #[serde(default)]
    content_type: String,
    #[serde(default)]
    body: String,
    #[serde(default)]
    body_b64: String,
    /// A zstd skippable frame of this many zero bytes goes before the body.
    #[serde(default)]
    skip_prefix: usize,
    #[serde(default)]
    headers: BTreeMap<String, String>,
    #[serde(default)]
    upstreams: Vec<Upstream>,
    status: u16,
    response_headers: BTreeMap<String, String>,
    #[serde(default)]
    response: String,
    #[serde(default)]
    response_b64: String,
    requests: Vec<String>,
}

/// The kept response headers, in Go's canonical spelling.
const KEEP: [&str; 6] = [
    "Content-Type",
    "Cache-Control",
    "Content-Disposition",
    "Content-Length",
    "Etag",
    "Last-Modified",
];

/// Scenarios whose Go error message is Go's own dial error text: the status, headers and
/// the error envelope's `type` and `code` are compared, and the message must be a string.
const STATUS_ONLY: [&str; 1] = ["video_content_connect_refused"];

/// Scenarios whose Go error copied the stream's own `Content-Length: 0` from its
/// passthrough headers, which cuts Go's error body to nothing. Rust drops framing
/// headers from them (`respond::write_error_headers`), so length and body are skipped.
const ADDON_FRAMING: [&str; 1] = ["compat_stream_empty"];

/// The error envelope check for a [`STATUS_ONLY`] scenario, `None` when it matches Go.
fn gateway_error_diff(go: &str, rust: &str) -> Option<String> {
    let parse = |s: &str| serde_json::from_str::<serde_json::Value>(s).ok();
    let want = parse(go).expect("Go error body is JSON");
    assert!(
        want["error"]["type"].is_string() && want["error"]["code"].is_string(),
        "{go}"
    );
    let Some(got) = parse(rust) else {
        return Some(format!("body is not JSON: {rust}"));
    };
    let stable = |v: &serde_json::Value| (v["error"]["type"].clone(), v["error"]["code"].clone());
    if stable(&got) != stable(&want) || !got["error"]["message"].is_string() {
        return Some(format!("error envelope:\n  go   {go}\n  rust {rust}"));
    }
    None
}

/// Answers each connection with the next scripted reply and records the raw request,
/// like the Go driver's capture server.
#[derive(Default)]
struct Capture {
    replies: Mutex<VecDeque<Upstream>>,
    requests: Mutex<Vec<String>>,
}

async fn capture(listener: tokio::net::TcpListener, state: Arc<Capture>, addr: String) {
    loop {
        let Ok((conn, _)) = listener.accept().await else { return };
        let (state, addr) = (state.clone(), addr.clone());
        tokio::spawn(async move {
            let mut reader = BufReader::new(conn);
            let mut raw = Vec::new();
            let mut length = 0;
            loop {
                let mut line = Vec::new();
                if reader.read_until(b'\n', &mut line).await.unwrap_or(0) == 0 {
                    return;
                }
                raw.extend_from_slice(&line);
                let text = String::from_utf8_lossy(&line).to_ascii_lowercase();
                if let Some(value) = text.strip_prefix("content-length:") {
                    length = value.trim().parse().unwrap_or(0);
                }
                if line == b"\r\n" {
                    break;
                }
            }
            let mut body = vec![0; length];
            let _ = reader.read_exact(&mut body).await;
            raw.extend_from_slice(&body);
            state
                .requests
                .lock()
                .unwrap()
                .push(String::from_utf8_lossy(&raw).into_owned());
            let reply = state.replies.lock().unwrap().pop_front().unwrap_or(Upstream {
                delay_ms: 0,
                status: 599,
                headers: vec![],
                body: "no scripted reply".into(),
            });
            tokio::time::sleep(std::time::Duration::from_millis(reply.delay_ms)).await;
            let payload = reply.body.replace("UPSTREAM", &addr);
            let reason = axum::http::StatusCode::from_u16(reply.status)
                .ok()
                .and_then(|s| s.canonical_reason())
                .unwrap_or("");
            let mut out = format!("HTTP/1.1 {} {reason}\r\n", reply.status);
            for (name, value) in &reply.headers {
                out.push_str(&format!("{name}: {value}\r\n"));
            }
            out.push_str(&format!(
                "Content-Length: {}\r\nConnection: close\r\n\r\n{payload}",
                payload.len()
            ));
            let mut conn = reader.into_inner();
            let _ = conn.write_all(out.as_bytes()).await;
            let _ = conn.shutdown().await;
        });
    }
}

struct Normalizer {
    addr: String,
    boundary: Regex,
    created: Regex,
    video: Regex,
    key: Regex,
    keys: Mutex<Vec<String>>,
}

impl Normalizer {
    /// The Go driver's `normalize`: capture address, multipart writer boundary,
    /// clock-derived timestamps, generated video IDs, and the xAI keys as `XAI-KEY-<n>`
    /// in order of first use (their sort order follows the random capture port).
    fn apply(&self, s: &str) -> String {
        let s = s.replace(&self.addr, "UPSTREAM");
        let mut s = self
            .key
            .replace_all(&s, |c: &regex::Captures| {
                let mut keys = self.keys.lock().unwrap();
                let n = match keys.iter().position(|k| *k == c[0]) {
                    Some(i) => i + 1,
                    None => {
                        keys.push(c[0].to_owned());
                        keys.len()
                    }
                };
                format!("XAI-KEY-{n}")
            })
            .into_owned();
        if let Some(m) = self.boundary.captures(&s) {
            let boundary = m[1].to_owned();
            s = canonical_form(&s.replace(&boundary, "BOUNDARY"));
        }
        let s = self.created.replace_all(&s, |c: &regex::Captures| {
            if c[2].parse::<i64>().unwrap_or(0) >= 1_750_000_000 {
                format!("\"{}\":\"<now>\"", &c[1])
            } else {
                c[0].to_owned()
            }
        });
        self.video.replace_all(&s, "\"video_<id>\"").into_owned()
    }
}

/// The Go driver's `canonicalForm`: a rebuilt multipart body with its values, then its
/// files, sorted (Go writes each group in map order).
fn canonical_form(s: &str) -> String {
    const SEP: &str = "--BOUNDARY";
    let Some(i) = s.find("\r\n\r\n") else {
        return s.to_owned();
    };
    if !s[i + 4..].starts_with(&format!("{SEP}\r\n")) {
        return s.to_owned();
    }
    let (head, pieces) = (&s[..i + 4], s[i + 4..].split(SEP).collect::<Vec<_>>());
    if pieces.len() < 3 {
        return s.to_owned();
    }
    let (mut lead, mut values, mut files) = (Vec::new(), Vec::new(), Vec::new());
    for p in &pieces[1..pieces.len() - 1] {
        if p.contains("name=\"model\"\r\n") || p.contains("name=\"stream\"\r\n") {
            lead.push(*p);
        } else if p.contains("filename=") {
            files.push(*p);
        } else {
            values.push(*p);
        }
    }
    values.sort_unstable();
    files.sort_unstable();
    lead.extend(values);
    lead.extend(files);
    format!(
        "{head}{}{SEP}{}{SEP}{}",
        pieces[0],
        lead.join(SEP),
        pieces[pieces.len() - 1]
    )
}

/// The last xAI video builtin in a Grok Shell catalog.
const PREVIEW_ENTRY: &str = r#"{"id":"grok-imagine-video-1.5-preview","model":"grok-imagine-video-1.5-preview","name":"Grok Imagine Video 1.5 Preview","api_backend":"responses","supported_in_api":true}"#;

/// One process holds one registry overlay, so the two fixtures replay in turn.
#[tokio::test]
async fn media_routes_match_go() {
    let media: Fixture = serde_json::from_str(include_str!("fixtures/media_go.json")).expect("fixture parses");
    let speech: Fixture = serde_json::from_str(include_str!("fixtures/speech_go.json")).expect("fixture parses");
    // v8.0.35 adds the speech builtins to the xAI catalog (6a853dce): the 6fecc6e Grok
    // Shell catalogs expect them as the speech fixture recorded them, after the video
    // builtins.
    let catalog = &speech
        .scenarios
        .iter()
        .find(|s| s.name == "models_grok_shell")
        .expect("speech fixture lists the catalog")
        .response;
    let speech_entries = &catalog[catalog.find(r#"{"id":"grok-tts""#).expect("grok-tts listed")..catalog.len() - 2];
    let mut failures = replay("media", &media, |s| {
        if s.name.starts_with("models_grok_shell") {
            s.response
                .replace(PREVIEW_ENTRY, &format!("{PREVIEW_ENTRY},{speech_entries}"))
        } else {
            s.response.clone()
        }
    })
    .await;
    failures.extend(replay("speech", &speech, |s| s.response.clone()).await);
    let errors: Fixture =
        serde_json::from_str(include_str!("fixtures/passthrough_errors_go.json")).expect("fixture parses");
    failures.extend(replay("errors", &errors, |s| s.response.clone()).await);
    assert!(
        failures.is_empty(),
        "{} scenario(s) differ from Go:\n{}",
        failures.len(),
        failures.join("\n\n")
    );
}

/// Replays `fixture` on a fresh runtime; `expected` is the response a scenario must give.
async fn replay(tag: &str, fixture: &Fixture, expected: impl Fn(&Scenario) -> String) -> Vec<String> {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    let upstream = Arc::new(Capture::default());
    tokio::spawn(capture(listener, upstream.clone(), addr.clone()));

    let auth_dir = std::env::temp_dir().join(format!("cpa-{tag}-{}", std::process::id()));
    let config = fixture
        .config
        .replace("UPSTREAM", &addr)
        .replace("PORT", "0")
        .replace("AUTHDIR", &auth_dir.to_string_lossy());
    // Go reads the fixture's unset strategy as round-robin. cliproxy-rs reads it as
    // soonest-reset, whose one probe per key reorders the two xAI keys across models
    // (docs/DIFFERENCES-FROM-GO.md "Default routing strategy"), so Go's is set.
    assert!(!config.contains("strategy:"), "the fixture leaves the strategy unset");
    let config = format!("{config}\nrouting:\n  strategy: round-robin\n");
    let config = Config::parse(&config).expect("config parses");
    let credentials = cpa_core::config::credentials::load(&config);
    let executors = Executors {
        claude: ClaudeExecutor::new("http://127.0.0.1:1").unwrap(),
        codex: Default::default(),
        devices: Default::default(),
        openai: Default::default(),
        google: Default::default(),
    };
    let rt = Arc::new(cpa_server::testing::runtime(config, credentials, executors));
    cpa_server::install_registry(&rt);
    let server = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", server.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(server, router(rt)).await.unwrap() });

    let norm = Normalizer {
        addr: addr.clone(),
        boundary: Regex::new(r"boundary=([0-9a-f]{60})").unwrap(),
        created: Regex::new(r#""(created_at|created)":(1[7-9]\d{8})"#).unwrap(),
        video: Regex::new(r#""video_[0-9a-f]{32}""#).unwrap(),
        key: Regex::new(r"sk-fake-xai-[a-z]").unwrap(),
        keys: Mutex::default(),
    };
    let client = wreq::Client::new();
    let mut failures = Vec::new();
    for s in &fixture.scenarios {
        *upstream.replies.lock().unwrap() = s.upstreams.iter().cloned().collect();
        upstream.requests.lock().unwrap().clear();
        let method = wreq::Method::from_bytes(s.method.as_bytes()).unwrap();
        // The Go driver's header.Set calls: defaults, then the scenario's overrides.
        let mut headers = wreq::header::HeaderMap::new();
        let mut set = |name: &str, value: &str| {
            let name = wreq::header::HeaderName::from_bytes(name.as_bytes()).unwrap();
            headers.insert(name, value.parse().unwrap());
        };
        set("Authorization", "Bearer client-key-1");
        set("User-Agent", "media-golden/1");
        if !s.content_type.is_empty() {
            set("Content-Type", &s.content_type);
        }
        for (name, value) in &s.headers {
            set(name, value);
        }
        let mut req = client.request(method, format!("{base}{}", s.path)).headers(headers);
        if s.method == "POST" {
            let mut body = Vec::new();
            if s.skip_prefix > 0 {
                body.extend([0x50, 0x2a, 0x4d, 0x18]);
                body.extend((s.skip_prefix as u32).to_le_bytes());
                body.resize(body.len() + s.skip_prefix, 0);
            }
            body.extend(if s.body_b64.is_empty() {
                s.body.clone().into_bytes()
            } else {
                base64::engine::general_purpose::STANDARD.decode(&s.body_b64).unwrap()
            });
            req = req.body(body);
        }
        let res = req.send().await.unwrap_or_else(|e| panic!("{}: {e}", s.name));
        let status = res.status().as_u16();
        let mut headers = BTreeMap::new();
        for name in KEEP {
            if let Some(value) = res.headers().get(name) {
                let value = value.to_str().unwrap_or_default();
                if !value.is_empty() {
                    headers.insert(name.to_owned(), value.to_owned());
                }
            }
        }
        let body = res.bytes().await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        let (response, response_b64) = match std::str::from_utf8(&body) {
            Ok(text) => (norm.apply(text), String::new()),
            Err(_) => (String::new(), base64::engine::general_purpose::STANDARD.encode(&body)),
        };
        let requests: Vec<String> = upstream
            .requests
            .lock()
            .unwrap()
            .iter()
            .map(|r| norm.apply(r))
            .collect();

        let mut diffs = Vec::new();
        if status != s.status {
            diffs.push(format!("status: go {} rust {status}", s.status));
        }
        let framing = ADDON_FRAMING.contains(&s.name.as_str());
        let compare_body = !STATUS_ONLY.contains(&s.name.as_str()) && !framing;
        let mut want_headers = s.response_headers.clone();
        if !compare_body {
            headers.remove("Content-Length");
            want_headers.remove("Content-Length");
        }
        if headers != want_headers {
            diffs.push(format!("headers:\n  go   {:?}\n  rust {headers:?}", s.response_headers));
        }
        let want = expected(s);
        if compare_body && (response != want || response_b64 != s.response_b64) {
            diffs.push(format!("body:\n  go   {want}\n  rust {response}"));
        }
        if !compare_body
            && !framing
            && let Some(diff) = gateway_error_diff(&s.response, &response)
        {
            diffs.push(diff);
        }
        if requests != s.requests {
            diffs.push(format!(
                "upstream requests:\n  go   {:?}\n  rust {requests:?}",
                s.requests
            ));
        }
        if !diffs.is_empty() {
            failures.push(format!("== {tag}/{}\n{}", s.name, diffs.join("\n")));
        }
    }
    if tag == "speech" {
        // Past the shared request limit: Go has none by default, reads an identity body
        // whole and answers its decoded 1 MiB check.
        let want = fixture
            .scenarios
            .iter()
            .find(|s| s.name == "speech_zstd_decoded_too_large")
            .expect("the oversize scenario");
        let res = client
            .post(format!("{base}/v1/tts"))
            .header("Authorization", "Bearer client-key-1")
            .header("Content-Type", "application/json")
            .body(vec![b' '; cpa_server::MAX_REQUEST_BYTES + 1])
            .send()
            .await
            .unwrap();
        let status = res.status().as_u16();
        let body = res.text().await.unwrap();
        if (status, body.as_str()) != (want.status, want.response.as_str()) {
            failures.push(format!(
                "== speech/identity_body_over_request_limit\n  go   {} {}\n  rust {status} {body}",
                want.status, want.response
            ));
        }
    }
    failures
}
