//! Go's own Claude executor tests, replayed end to end. record.py records every
//! `Execute`, `ExecuteStream` and `CountTokens` call Go's tests make on a
//! `*ClaudeExecutor`: the config, credential, request and context inputs, every upstream
//! exchange (request as sent, response as read) and the result. Here the same inputs go
//! through the Rust executor against a local upstream that answers with Go's recorded
//! responses (plain HTTP for Go's test servers, TLS with a throwaway CA for the
//! Anthropic hosts Go's tests intercepted); what Rust sends and returns is compared
//! with what Go sent and returned. Nothing leaves loopback.

use std::collections::{BTreeMap, VecDeque};
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use bytes::Bytes;
use cpa_core::config::Config;
use cpa_core::credential::{Credential, Source};
use cpa_core::exec::{Caller, ExecError, ExecRequest, Operation, ResolvedModel, ResolvedSource, ResponseBody};
use cpa_core::format::Format;
use futures_util::StreamExt;
use serde_json::Value;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use super::ClaudeExecutor;

/// A request the Rust executor sent.
#[derive(Debug, Clone)]
struct Seen {
    method: String,
    target: String,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

/// A recorded Go response; `None` closes the connection unanswered (Go saw a
/// transport error).
type Reply = Option<Answer>;

struct Answer {
    status: u16,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
    end: End,
}

/// How the recorded body ended for Go's client.
enum End {
    Complete,
    /// The response declared this Content-Length and closed after a shorter body.
    Short(usize),
    /// A body read failed without a declared length: a chunked body that stops
    /// without its terminating chunk.
    Abort,
}

#[derive(Default)]
struct Script {
    replies: VecDeque<Reply>,
    seen: Vec<Seen>,
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

/// One HTTP/1.1 exchange on a connection, then close.
async fn serve<S: AsyncRead + AsyncWrite + Unpin>(mut stream: S, script: Arc<Mutex<Script>>) {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 8192];
    let head_end = loop {
        if let Some(end) = find(&buf, b"\r\n\r\n") {
            break end;
        }
        match stream.read(&mut chunk).await {
            Ok(0) | Err(_) => return,
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
        }
    };
    let head = String::from_utf8_lossy(&buf[..head_end]).into_owned();
    let mut lines = head.split("\r\n");
    let mut request_line = lines.next().unwrap_or_default().split(' ');
    let (method, target) = (
        request_line.next().unwrap_or_default().to_owned(),
        request_line.next().unwrap_or_default().to_owned(),
    );
    let headers: Vec<(String, String)> = lines
        .filter_map(|l| l.split_once(':'))
        .map(|(k, v)| (k.to_owned(), v.trim().to_owned()))
        .collect();
    let header = |name: &str| {
        headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.clone())
    };
    let mut body = buf[head_end + 4..].to_vec();
    if let Some(length) = header("content-length").and_then(|v| v.parse::<usize>().ok()) {
        while body.len() < length {
            match stream.read(&mut chunk).await {
                Ok(0) | Err(_) => break,
                Ok(n) => body.extend_from_slice(&chunk[..n]),
            }
        }
        body.truncate(length);
    } else if header("transfer-encoding").is_some_and(|v| v.eq_ignore_ascii_case("chunked")) {
        // Read to the terminating chunk, then decode.
        while find(&body, b"0\r\n\r\n").is_none() {
            match stream.read(&mut chunk).await {
                Ok(0) | Err(_) => break,
                Ok(n) => body.extend_from_slice(&chunk[..n]),
            }
        }
        let (mut decoded, mut rest) = (Vec::new(), body.as_slice());
        while let Some(line_end) = find(rest, b"\r\n") {
            let size = usize::from_str_radix(String::from_utf8_lossy(&rest[..line_end]).trim(), 16).unwrap_or(0);
            if size == 0 || rest.len() < line_end + 2 + size {
                break;
            }
            decoded.extend_from_slice(&rest[line_end + 2..line_end + 2 + size]);
            rest = &rest[(line_end + 4 + size).min(rest.len())..];
        }
        body = decoded;
    }
    let reply = {
        let mut script = script.lock().unwrap();
        script.seen.push(Seen {
            method,
            target,
            headers,
            body,
        });
        script.replies.pop_front().unwrap_or_else(|| {
            Some(Answer {
                status: 599,
                headers: vec![("Content-Type".into(), "text/plain".into())],
                body: b"no recorded Go response for this request".to_vec(),
                end: End::Complete,
            })
        })
    };
    let Some(Answer {
        status,
        headers: reply_headers,
        body: reply_body,
        end,
    }) = reply
    else {
        return;
    };
    let reason = http::StatusCode::from_u16(status)
        .ok()
        .and_then(|s| s.canonical_reason())
        .unwrap_or("Status");
    let mut out = format!("HTTP/1.1 {status} {reason}\r\n");
    for (name, value) in &reply_headers {
        if !["content-length", "transfer-encoding", "connection", "date"]
            .iter()
            .any(|h| name.eq_ignore_ascii_case(h))
        {
            out.push_str(&format!("{name}: {value}\r\n"));
        }
    }
    let body = match end {
        End::Complete => {
            out.push_str(&format!("Content-Length: {}\r\n", reply_body.len()));
            reply_body
        }
        End::Short(declared) => {
            out.push_str(&format!("Content-Length: {declared}\r\n"));
            reply_body
        }
        End::Abort => {
            out.push_str("Transfer-Encoding: chunked\r\n");
            let mut chunked = Vec::new();
            if !reply_body.is_empty() {
                chunked.extend_from_slice(format!("{:x}\r\n", reply_body.len()).as_bytes());
                chunked.extend_from_slice(&reply_body);
                chunked.extend_from_slice(b"\r\n");
            }
            chunked
        }
    };
    out.push_str("Connection: close\r\n\r\n");
    let _ = stream.write_all(out.as_bytes()).await;
    let _ = stream.write_all(&body).await;
    let _ = stream.shutdown().await;
}

/// A plain and a TLS listener sharing one script.
struct Upstream {
    plain: SocketAddr,
    tls: SocketAddr,
    ca: Vec<u8>,
    script: Arc<Mutex<Script>>,
}

impl Upstream {
    async fn start(hosts: &[String], replies: VecDeque<Reply>) -> Self {
        let script = Arc::new(Mutex::new(Script {
            replies,
            seen: Vec::new(),
        }));
        let plain = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let secure = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let (plain_addr, tls_addr) = (plain.local_addr().unwrap(), secure.local_addr().unwrap());
        let (ca, acceptor) = crate::test_tls::acceptor(hosts, b"\x08http/1.1");
        let acceptor = Arc::new(acceptor);
        let shared = script.clone();
        tokio::spawn(async move {
            while let Ok((socket, _)) = plain.accept().await {
                tokio::spawn(serve(socket, shared.clone()));
            }
        });
        let shared = script.clone();
        tokio::spawn(async move {
            while let Ok((socket, _)) = secure.accept().await {
                let (acceptor, shared) = (acceptor.clone(), shared.clone());
                tokio::spawn(async move {
                    let ssl = btls::ssl::Ssl::new(acceptor.context()).unwrap();
                    let mut stream = tokio_btls::SslStream::new(ssl, socket).unwrap();
                    if std::pin::Pin::new(&mut stream).accept().await.is_ok() {
                        serve(stream, shared).await;
                    }
                });
            }
        });
        Self {
            plain: plain_addr,
            tls: tls_addr,
            ca,
            script,
        }
    }
}

fn bytes(value: &Value) -> Vec<u8> {
    use base64::Engine;
    match value {
        Value::String(s) => s.as_bytes().to_vec(),
        Value::Object(o) => o
            .get("b64")
            .and_then(Value::as_str)
            .map(|b| base64::engine::general_purpose::STANDARD.decode(b).unwrap())
            .unwrap_or_default(),
        _ => Vec::new(),
    }
}

/// Go's `http.Header` (canonical names, value lists) as ordered pairs.
fn header_pairs(value: &Value) -> Vec<(String, String)> {
    value
        .as_object()
        .into_iter()
        .flatten()
        .flat_map(|(k, vs)| {
            vs.as_array()
                .into_iter()
                .flatten()
                .map(move |v| (k.clone(), v.as_str().unwrap_or_default().to_owned()))
        })
        .collect()
}

/// Wall-clock values and the body-dependent CCH, which differ in both implementations
/// whenever the body carries a random identifier.
fn normalize(text: &str) -> String {
    use std::sync::OnceLock;
    static RULES: OnceLock<Vec<(regex::Regex, &'static str)>> = OnceLock::new();
    let rules = RULES.get_or_init(|| {
        [
            (r"cch=[0-9a-f]{5};", "cch=<cch>;"),
            (r"Today's date is \d{4}-\d{2}-\d{2}", "Today's date is <date>"),
            // Translators stamp responses with the wall clock.
            (r#""(created|created_at)":\d+"#, r#""$1":<unix>"#),
            (r#""createTime":"[0-9T:.Z-]+""#, r#""createTime":"<time>""#),
        ]
        .into_iter()
        .map(|(re, to)| (regex::Regex::new(re).unwrap(), to))
        .collect()
    });
    let mut out = text.to_owned();
    for (re, to) in rules {
        out = re.replace_all(&out, *to).into_owned();
    }
    out
}

/// UUID- and 64-hex-shaped identifiers.
fn id_pattern() -> &'static regex::Regex {
    use std::sync::OnceLock;
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    RE.get_or_init(|| {
        regex::Regex::new(r"[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}|[0-9a-f]{64}").unwrap()
    })
}

fn ids<'a>(texts: impl IntoIterator<Item = &'a str>) -> std::collections::HashSet<String> {
    texts
        .into_iter()
        .flat_map(|t| id_pattern().find_iter(t).map(|m| m.as_str().to_owned()))
        .collect()
}

/// Masks the identifiers only one implementation produced (random session, device and
/// prompt IDs), numbered by first appearance so that reuse still has to line up. An
/// identifier from the recorded inputs, or one both implementations produced, stays
/// verbatim: a wrong account UUID or device ID shows.
// ponytail: a derived identifier that both implementations compute but get different
// is masked like a random one; the derivations have their own unit tests.
struct Masker<'a> {
    keep: &'a std::collections::HashSet<String>,
    numbered: std::collections::HashMap<String, usize>,
}

impl<'a> Masker<'a> {
    fn new(keep: &'a std::collections::HashSet<String>) -> Self {
        Self {
            keep,
            numbered: Default::default(),
        }
    }

    fn apply(&mut self, text: &str) -> String {
        let text = normalize(text);
        let mut out = String::with_capacity(text.len());
        let mut last = 0;
        for m in id_pattern().find_iter(&text) {
            out.push_str(&text[last..m.start()]);
            if self.keep.contains(m.as_str()) {
                out.push_str(m.as_str());
            } else {
                let next = self.numbered.len() + 1;
                let n = *self.numbered.entry(m.as_str().to_owned()).or_insert(next);
                out.push_str(&format!("<id{n}>"));
            }
            last = m.end();
        }
        out.push_str(&text[last..]);
        out
    }
}

fn text(b: &[u8]) -> String {
    String::from_utf8_lossy(b).into_owned()
}

/// Rewrites Go's local test-server origins to the Rust upstream.
fn relocate(value: &str, go_origins: &regex::Regex, plain: SocketAddr) -> String {
    go_origins.replace_all(value, format!("http://{plain}")).into_owned()
}

fn relocate_json(value: &Value, go_origins: &regex::Regex, plain: SocketAddr) -> Value {
    match value {
        Value::String(s) => Value::String(relocate(s, go_origins, plain)),
        Value::Array(a) => Value::Array(a.iter().map(|v| relocate_json(v, go_origins, plain)).collect()),
        Value::Object(o) => Value::Object(
            o.iter()
                .map(|(k, v)| (k.clone(), relocate_json(v, go_origins, plain)))
                .collect(),
        ),
        other => other.clone(),
    }
}

/// Go's format strings; an empty source is Go's zero `Format`, which no pair matches.
fn format(name: &str) -> Format {
    Format::parse(name).unwrap_or(Format::Claude)
}

fn resolved(metadata: &Value) -> Option<ResolvedModel> {
    let pick = |key: &str, source| {
        let info = metadata.get(key)?.as_object()?.clone();
        Some(ResolvedModel {
            info: cpa_core::registry::ModelInfo::from_raw(info).ok()?,
            source,
        })
    };
    pick("cliproxy.resolved_api_key_model_info", ResolvedSource::ApiKey)
        .or_else(|| pick("cliproxy.resolved_codex_oauth_model_info", ResolvedSource::CodexOAuth))
}

/// The outcome both implementations report.
#[derive(Debug, PartialEq)]
struct Outcome {
    payload: String,
    error: Option<Failure>,
}

/// What a client and the scheduler see of an executor error.
#[derive(Debug, PartialEq)]
struct Failure {
    /// Go's `StatusCode()`; a missing or zero status answers 500.
    status: u16,
    /// How the conductor treats the failure: `request` (answer the caller),
    /// `credential` (`IsCredentialScoped`) or `not-request` (fail over). A Go error
    /// without `IsRequestScoped` is classified by its status, as the Rust scheduler's
    /// status rule does.
    scope: &'static str,
    /// The client-visible text: a direct answer's body, else the error message.
    message: String,
}

impl Outcome {
    fn masked(&self, masker: &mut Masker<'_>) -> Self {
        Self {
            payload: masker.apply(&self.payload),
            error: self.error.as_ref().map(|e| Failure {
                status: e.status,
                scope: e.scope,
                message: masker.apply(&e.message),
            }),
        }
    }
}

fn go_error(info: &Value) -> Option<Failure> {
    let message = info.get("message")?.as_str()?.to_owned();
    let status = info["status"].as_u64().filter(|s| *s != 0).unwrap_or(500) as u16;
    let scope = if info["request_scoped"].as_bool() == Some(true) {
        "request"
    } else if info["credential_scoped"].as_bool() == Some(true) {
        "credential"
    } else if info["request_scoped"].is_null()
        && crate::upstream::scope_for(status) == cpa_core::exec::FailureScope::Request
    {
        "request"
    } else {
        "not-request"
    };
    // claudeFastDirectResponseError: the client receives the upstream status and body.
    let (status, message) = match info.get("direct_status").and_then(Value::as_u64) {
        Some(direct) => (direct as u16, text(&bytes(&info["direct_body"]))),
        None => (status, message),
    };
    Some(Failure { status, scope, message })
}

fn rust_error(error: &ExecError, go: Option<&Failure>) -> Failure {
    use cpa_core::exec::FailureScope;
    let scope = match error.scope {
        FailureScope::Request => "request",
        FailureScope::Credential if go.is_some_and(|g| g.scope == "credential") => "credential",
        _ => "not-request",
    };
    Failure {
        status: error.status,
        scope,
        message: text(&error.body),
    }
}

/// Go's OpenAI Chat stream chunks are bare JSON objects (the handler frames them); the
/// Rust executor yields the framed events.
fn unframe_chat(stream: &str) -> String {
    stream
        .lines()
        .filter_map(|l| l.strip_prefix("data: "))
        .filter(|d| *d != "[DONE]")
        .collect()
}

/// Go tests whose upstream body key order comes from Go's random map iteration (several
/// payload-rule params in one Go map): their bodies compare as JSON values.
const GO_MAP_ORDER: &[&str] = &["TestClaudeExecutorPayloadOverrideDisabledThinking"];

/// Go tests whose recorded exchange a replay cannot reproduce, with the reason.
const NOT_REPLAYABLE: &[(&str, &str)] = &[
    (
        "TestClaudeExecutor_ExecuteStreamOAuthCancellationIsRequestScoped",
        "cancels the request context while the upstream holds the stream open",
    ),
    (
        "TestClaudeExecutor_ExecuteStreamOAuthStartupCancellationIsRequestScoped",
        "cancels the request context before the upstream answers",
    ),
];

/// A replay's verdict when Rust matches Go.
pub(super) enum Verdict {
    Matched,
    /// Not replayed, for this reason (see [`NOT_REPLAYABLE`]).
    Unverified(&'static str),
}

/// Replays one recorded executor call; `Err` lists how Rust differs from Go.
pub(super) async fn replay(test: &str, record: &Value) -> Result<Verdict, String> {
    if let Some((_, why)) = NOT_REPLAYABLE.iter().find(|(name, _)| *name == test) {
        return Ok(Verdict::Unverified(why));
    }
    let exchanges = record["exchanges"]["exchanges"].as_array().cloned().unwrap_or_default();
    let go_origins = regex::Regex::new(r"http://127\.0\.0\.1:\d+").unwrap();
    // The default User-Agent names the build: CLIProxyAPI/dev in Go's test binary.
    let version = regex::Regex::new(r"^CLIProxyAPI/\S+").unwrap();
    let mut hosts: Vec<String> = [
        "api.anthropic.com",
        "platform.claude.com",
        "console.anthropic.com",
        "claude.ai",
    ]
    .map(str::to_owned)
    .to_vec();
    for exchange in &exchanges {
        if let Ok(url) = url::Url::parse(exchange["url"].as_str().unwrap_or_default())
            && url.scheme() == "https"
            && let Some(host) = url.host_str()
            && !hosts.iter().any(|h| h == host)
        {
            hosts.push(host.to_owned());
        }
    }
    let replies: VecDeque<Reply> = exchanges
        .iter()
        .map(|e| {
            (e["error"].as_str().is_none()).then(|| {
                let headers = header_pairs(&e["response_headers"]);
                let body = bytes(&e["response_body"]);
                let declared = headers
                    .iter()
                    .find(|(k, _)| k.eq_ignore_ascii_case("content-length"))
                    .and_then(|(_, v)| v.parse::<usize>().ok());
                let end = match declared {
                    Some(declared) if declared > body.len() => End::Short(declared),
                    _ if e["read_error"].as_str().is_some() => End::Abort,
                    _ => End::Complete,
                };
                Answer {
                    status: e["status"].as_u64().unwrap_or(200) as u16,
                    headers,
                    body,
                    end,
                }
            })
        })
        .collect();
    let upstream = Upstream::start(&hosts, replies).await;
    let config = relocate(
        record["config"].as_str().unwrap_or_default(),
        &go_origins,
        upstream.plain,
    );
    let cfg = Config::parse(&config).map_err(|e| format!("config does not parse: {e:?}"))?;
    let auth = relocate_json(&record["auth"], &go_origins, upstream.plain);
    let mut credential = Credential {
        id: auth["id"].as_str().unwrap_or_default().to_owned(),
        provider: auth["provider"].as_str().unwrap_or_default().to_owned(),
        source: Source::Config {
            section: "go-test".into(),
            index: 0,
        },
        disabled: auth["disabled"].as_bool().unwrap_or(false),
        label: auth["label"].as_str().unwrap_or_default().to_owned(),
        attributes: auth["attributes"]
            .as_object()
            .into_iter()
            .flatten()
            .map(|(k, v)| (k.clone(), v.as_str().unwrap_or_default().to_owned()))
            .collect(),
        metadata: auth["metadata"].as_object().cloned().unwrap_or_default(),
        revision: 0,
    };
    let hooks = crate::proxy::Hooks {
        trust: Some(wreq::tls::trust::CertStore::from_pem_stack(upstream.ca.clone()).unwrap()),
        resolve: hosts.iter().map(|h| (h.clone(), upstream.tls)).collect(),
    };
    let executor = ClaudeExecutor::with_hooks(hooks, super::DEFAULT_BASE_URL);
    if crate::oauth::needs_identity(&credential) && executor.needs_prepare(&credential, &cfg) {
        // The runtime prepares before dispatch when readiness is PrepareNow.
        if let Ok(patch) = executor.prepare(&credential, &cfg).await {
            patch.apply(&mut credential.metadata);
        }
    }
    let request = &record["request"];
    let options = &record["options"];
    let metadata = &options["metadata"];
    let payload = Bytes::from(bytes(&request["payload"]));
    let original = bytes(&options["original_request"]);
    let headers: http::HeaderMap = header_pairs(&record["headers"])
        .into_iter()
        .filter_map(|(k, v)| {
            Some((
                http::HeaderName::try_from(k).ok()?,
                http::HeaderValue::try_from(v).ok()?,
            ))
        })
        .collect();
    let kind = record["kind"].as_str().unwrap();
    let source = format(options["source_format"].as_str().unwrap_or_default());
    let execution = metadata["execution_session_id"].as_str().map(str::to_owned);
    let session = Some(cpa_common::session::extract_session_id(
        &headers,
        &payload,
        &cpa_common::session::Meta {
            execution_session: execution.as_deref(),
            derived: None,
        },
    ))
    .filter(|s| !s.is_empty())
    .map(|s| cpa_common::session::bound_session_identity(&s));
    let model = request["model"].as_str().unwrap_or_default().to_owned();
    let req = ExecRequest {
        operation: if kind == "count_tokens" {
            Operation::CountTokens
        } else {
            Operation::Generate
        },
        source_format: source,
        response_format: options["response_format"]
            .as_str()
            .and_then(Format::parse)
            .unwrap_or(source),
        requested_model: metadata["requested_model"].as_str().unwrap_or(&model).to_owned(),
        model,
        original_body: if original.is_empty() {
            payload.clone()
        } else {
            Bytes::from(original)
        },
        body: payload,
        stream: kind == "execute_stream",
        alt: options["alt"].as_str().filter(|a| !a.is_empty()).map(str::to_owned),
        session,
        execution_session: execution,
        derived_session: None,
        request_path: metadata["request_path"].as_str().unwrap_or_default().to_owned(),
        resolved_model: resolved(&request["metadata"]),
        usage: Default::default(),
        headers,
        caller: Caller {
            principal: record["caller"].as_str().unwrap_or_default().to_owned(),
            source: "authorization",
        },
    };
    let chat = req.response_format == Format::OpenAI && kind == "execute_stream";
    let result = executor.execute(&credential, req, &cfg).await;
    let recorded = &record["result"];
    let go = if kind == "execute_stream" && recorded["error"].is_null() {
        let chunks = record["exchanges"]["chunks"].as_array().cloned().unwrap_or_default();
        let payload: Vec<u8> = chunks.iter().flat_map(|c| bytes(&c["payload"])).collect();
        Outcome {
            payload: text(&payload),
            error: chunks.iter().find_map(|c| go_error(&c["error"])),
        }
    } else {
        Outcome {
            payload: text(&bytes(&recorded["payload"])),
            error: go_error(&recorded["error"]),
        }
    };
    let rust = match result {
        Err(error) => Outcome {
            payload: String::new(),
            error: Some(rust_error(&error, go.error.as_ref())),
        },
        Ok(response) => match response.body {
            ResponseBody::Buffered(body) => Outcome {
                payload: text(&body),
                error: None,
            },
            ResponseBody::Stream(mut events) => {
                let (mut payload, mut error) = (Vec::new(), None);
                while let Some(event) = events.next().await {
                    match event {
                        Ok(chunk) => payload.extend_from_slice(&chunk),
                        Err(e) => {
                            error = Some(rust_error(&e, go.error.as_ref()));
                            break;
                        }
                    }
                }
                let payload = text(&payload);
                Outcome {
                    payload: if chat { unframe_chat(&payload) } else { payload },
                    error,
                }
            }
        },
    };
    let mut diffs = Vec::new();
    let seen = upstream.script.lock().unwrap().seen.clone();
    if seen.len() != exchanges.len() {
        diffs.push(format!(
            "upstream requests: rust sent {}, go sent {} ({:?} vs {:?})",
            seen.len(),
            exchanges.len(),
            seen.iter().map(|s| s.target.as_str()).collect::<Vec<_>>(),
            exchanges
                .iter()
                .map(|e| e["url"].as_str().unwrap_or_default())
                .collect::<Vec<_>>()
        ));
    }
    // Each exchange as both sides sent it: headers Go's executor set (the transport adds
    // Host, Content-Length, Accept-Encoding and its default User-Agent), then the body.
    type Headers = BTreeMap<String, Vec<String>>;
    /// Rust's headers, Go's headers, Rust's body, Go's body.
    type Sent = (Headers, Headers, String, String);
    let mut sent: Vec<Sent> = Vec::new();
    for (i, (rust, go)) in seen.iter().zip(&exchanges).enumerate() {
        let url = url::Url::parse(go["url"].as_str().unwrap_or_default()).unwrap();
        let go_target = match url.query() {
            Some(q) => format!("{}?{q}", url.path()),
            None => url.path().to_owned(),
        };
        if rust.method != go["method"].as_str().unwrap_or_default() || rust.target != go_target {
            diffs.push(format!(
                "exchange {i}: {} {} vs go {} {go_target}",
                rust.method, rust.target, go["method"]
            ));
        }
        let transport = ["host", "content-length", "accept-encoding", "user-agent", "connection"];
        let value = |v: &str| version.replace(v, "CLIProxyAPI/<version>").into_owned();
        let mut want = Headers::new();
        for (k, v) in header_pairs(&go["headers"]) {
            want.entry(k.to_lowercase()).or_default().push(value(&v));
        }
        let mut got = Headers::new();
        for (k, v) in &rust.headers {
            let k = k.to_lowercase();
            if want.contains_key(&k) || !transport.contains(&k.as_str()) {
                got.entry(k).or_default().push(value(v));
            }
        }
        // Random per request in both implementations.
        let random = "x-client-request-id";
        if want.contains_key(random) && got.contains_key(random) {
            want.remove(random);
            got.remove(random);
        }
        sent.push((got, want, text(&rust.body), text(&bytes(&go["body"]))));
    }
    // Identifiers to keep verbatim: the recorded inputs (config, credential, request,
    // upstream replies) and whatever both implementations produced.
    let mut inputs = record.clone();
    if let Some(object) = inputs.as_object_mut() {
        object.remove("result");
        object.remove("exchanges");
    }
    let replies: Vec<String> = exchanges
        .iter()
        .flat_map(|e| [e["response_headers"].to_string(), text(&bytes(&e["response_body"]))])
        .collect();
    let outputs = |outcome: &Outcome, side: fn(&Sent) -> (&Headers, &String)| {
        let mut texts = vec![outcome.payload.clone()];
        texts.extend(outcome.error.iter().map(|e| e.message.clone()));
        for exchange in &sent {
            let (headers, body) = side(exchange);
            texts.extend(headers.values().flatten().cloned());
            texts.push(body.clone());
        }
        ids(texts.iter().map(String::as_str))
    };
    let rust_ids = outputs(&rust, |(got, _, body, _)| (got, body));
    let go_ids = outputs(&go, |(_, want, _, body)| (want, body));
    let input_text = inputs.to_string();
    let mut keep = ids(std::iter::once(input_text.as_str()).chain(replies.iter().map(String::as_str)));
    keep.extend(rust_ids.intersection(&go_ids).cloned());
    let (mut rust_mask, mut go_mask) = (Masker::new(&keep), Masker::new(&keep));
    let (rust, go) = (rust.masked(&mut rust_mask), go.masked(&mut go_mask));
    if rust != go {
        diffs.push(format!("result:\n   rust: {rust:?}\n     go: {go:?}"));
    }
    for (i, (got, want, rust_body, go_body)) in sent.iter().enumerate() {
        let mask = |headers: &Headers, masker: &mut Masker<'_>| -> Headers {
            headers
                .iter()
                .map(|(k, vs)| (k.clone(), vs.iter().map(|v| masker.apply(v)).collect()))
                .collect()
        };
        let (got, want) = (mask(got, &mut rust_mask), mask(want, &mut go_mask));
        if got != want {
            let only_rust: Vec<_> = got.iter().filter(|(k, v)| want.get(*k) != Some(v)).collect();
            let only_go: Vec<_> = want.iter().filter(|(k, v)| got.get(*k) != Some(v)).collect();
            diffs.push(
                format!("exchange {i} headers:\n   rust: {only_rust:?}\n     go: {only_go:?}")
                    .replace("sk-ant-", "<sk>-"),
            );
        }
        let (rust_body, go_body) = (rust_mask.apply(rust_body), go_mask.apply(go_body));
        let as_json = |b: &str| serde_json::from_str::<Value>(b).ok();
        let same = rust_body == go_body
            || (GO_MAP_ORDER.contains(&test)
                && as_json(&rust_body).is_some()
                && as_json(&rust_body) == as_json(&go_body));
        if !same {
            diffs.push(format!("exchange {i} body:\n   rust: {rust_body}\n     go: {go_body}"));
        }
    }
    if diffs.is_empty() {
        Ok(Verdict::Matched)
    } else {
        Err(diffs.join("\n"))
    }
}
