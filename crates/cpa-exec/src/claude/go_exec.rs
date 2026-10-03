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
type Reply = Option<(u16, Vec<(String, String)>, Vec<u8>)>;

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
            Some((
                599,
                vec![("Content-Type".into(), "text/plain".into())],
                b"no recorded Go response for this request".to_vec(),
            ))
        })
    };
    let Some((status, reply_headers, reply_body)) = reply else {
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
    out.push_str(&format!(
        "Content-Length: {}\r\nConnection: close\r\n\r\n",
        reply_body.len()
    ));
    let _ = stream.write_all(out.as_bytes()).await;
    let _ = stream.write_all(&reply_body).await;
    let _ = stream.shutdown().await;
}

/// A throwaway CA and a leaf for `hosts`: (CA PEM, acceptor).
fn tls(hosts: &[String]) -> (Vec<u8>, btls::ssl::SslAcceptor) {
    use btls::asn1::Asn1Time;
    use btls::bn::BigNum;
    use btls::ec::{EcGroup, EcKey};
    use btls::hash::MessageDigest;
    use btls::nid::Nid;
    use btls::pkey::PKey;
    use btls::ssl::{SslAcceptor, SslMethod};
    use btls::x509::extension::{BasicConstraints, SubjectAlternativeName};
    use btls::x509::{X509, X509NameBuilder};
    let group = EcGroup::from_curve_name(Nid::X9_62_PRIME256V1).unwrap();
    let key = |_: ()| PKey::from_ec_key(EcKey::generate(&group).unwrap()).unwrap();
    let name = |cn: &str| {
        let mut n = X509NameBuilder::new().unwrap();
        n.append_entry_by_text("CN", cn).unwrap();
        n.build()
    };
    let (ca_key, leaf_key) = (key(()), key(()));
    let ca_name = name("cliproxy-rs Go replay CA");
    let mut ca = X509::builder().unwrap();
    ca.set_version(2).unwrap();
    ca.set_serial_number(&BigNum::from_u32(1).unwrap().to_asn1_integer().unwrap())
        .unwrap();
    ca.set_subject_name(&ca_name).unwrap();
    ca.set_issuer_name(&ca_name).unwrap();
    ca.set_pubkey(&ca_key).unwrap();
    ca.set_not_before(&Asn1Time::days_from_now(0).unwrap()).unwrap();
    ca.set_not_after(&Asn1Time::days_from_now(2).unwrap()).unwrap();
    ca.append_extension(&BasicConstraints::new().critical().ca().build().unwrap())
        .unwrap();
    ca.sign(&ca_key, MessageDigest::sha256()).unwrap();
    let ca = ca.build();
    let mut leaf = X509::builder().unwrap();
    leaf.set_version(2).unwrap();
    leaf.set_serial_number(&BigNum::from_u32(2).unwrap().to_asn1_integer().unwrap())
        .unwrap();
    leaf.set_subject_name(&name(&hosts[0])).unwrap();
    leaf.set_issuer_name(&ca_name).unwrap();
    leaf.set_pubkey(&leaf_key).unwrap();
    leaf.set_not_before(&Asn1Time::days_from_now(0).unwrap()).unwrap();
    leaf.set_not_after(&Asn1Time::days_from_now(2).unwrap()).unwrap();
    let mut san = SubjectAlternativeName::new();
    for host in hosts {
        san.dns(host);
    }
    let san = san.build(&leaf.x509v3_context(Some(&ca), None)).unwrap();
    leaf.append_extension(&san).unwrap();
    leaf.sign(&ca_key, MessageDigest::sha256()).unwrap();
    let leaf = leaf.build();
    let mut acceptor = SslAcceptor::mozilla_intermediate_v5(SslMethod::tls()).unwrap();
    acceptor.set_private_key(&leaf_key).unwrap();
    acceptor.set_certificate(&leaf).unwrap();
    acceptor.set_alpn_select_callback(|_, client| {
        btls::ssl::select_next_proto(b"\x08http/1.1", client).ok_or(btls::ssl::AlpnError::NOACK)
    });
    (ca.to_pem().unwrap(), acceptor.build())
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
        let (ca, acceptor) = tls(hosts);
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

/// Values that are random or wall-clock dependent in both implementations.
pub(super) fn normalize(text: &str) -> String {
    use std::sync::OnceLock;
    static RULES: OnceLock<Vec<(regex::Regex, &'static str)>> = OnceLock::new();
    let rules = RULES.get_or_init(|| {
        [
            (r"[0-9a-f]{64}", "<hex64>"),
            (
                r"[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}",
                "<uuid>",
            ),
            (r"cch=[0-9a-f]{5};", "cch=<cch>;"),
            (r"Today's date is \d{4}-\d{2}-\d{2}", "Today's date is <date>"),
            (r"user_[0-9a-f]{64}_account_", "user_<hex64>_account_"),
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

/// The outcome both implementations report, normalized for comparison.
#[derive(Debug, PartialEq)]
struct Outcome {
    payload: String,
    error: Option<(u16, bool, String)>,
}

fn go_error(info: &Value) -> Option<(u16, bool, String)> {
    let message = info.get("message")?.as_str()?.to_owned();
    let status = info["status"].as_u64().unwrap_or(500) as u16;
    let request_scoped = info["request_scoped"].as_bool().unwrap_or(false);
    Some((status, request_scoped, normalize(&message)))
}

fn rust_error(error: &ExecError) -> (u16, bool, String) {
    (
        error.status,
        error.scope == cpa_core::exec::FailureScope::Request,
        normalize(&text(&error.body)),
    )
}

/// Replays one recorded executor call; `Err` lists how Rust differs from Go.
pub(super) async fn replay(record: &Value) -> Result<(), String> {
    let exchanges = record["exchanges"]["exchanges"].as_array().cloned().unwrap_or_default();
    let go_origins = regex::Regex::new(r"http://127\.0\.0\.1:\d+").unwrap();
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
                (
                    e["status"].as_u64().unwrap_or(200) as u16,
                    header_pairs(&e["response_headers"]),
                    bytes(&e["response_body"]),
                )
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
    let result = executor.execute(&credential, req, &cfg).await;
    let rust = match result {
        Err(error) => Outcome {
            payload: String::new(),
            error: Some(rust_error(&error)),
        },
        Ok(response) => match response.body {
            ResponseBody::Buffered(body) => Outcome {
                payload: normalize(&text(&body)),
                error: None,
            },
            ResponseBody::Stream(mut events) => {
                let (mut payload, mut error) = (Vec::new(), None);
                while let Some(event) = events.next().await {
                    match event {
                        Ok(chunk) => payload.extend_from_slice(&chunk),
                        Err(e) => {
                            error = Some(rust_error(&e));
                            break;
                        }
                    }
                }
                Outcome {
                    payload: normalize(&text(&payload)),
                    error,
                }
            }
        },
    };
    let result = &record["result"];
    let go = if kind == "execute_stream" && result["error"].is_null() {
        let chunks = record["exchanges"]["chunks"].as_array().cloned().unwrap_or_default();
        let payload: Vec<u8> = chunks.iter().flat_map(|c| bytes(&c["payload"])).collect();
        Outcome {
            payload: normalize(&text(&payload)),
            error: chunks.iter().find_map(|c| go_error(&c["error"])),
        }
    } else {
        Outcome {
            payload: normalize(&text(&bytes(&result["payload"]))),
            error: go_error(&result["error"]),
        }
    };
    let mut diffs = Vec::new();
    if rust != go {
        diffs.push(format!("result:\n   rust: {rust:?}\n     go: {go:?}"));
    }
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
        // Go's recorded header map is what the executor set; the transport adds
        // Host, Content-Length, Accept-Encoding and its default User-Agent.
        let transport = ["host", "content-length", "accept-encoding", "user-agent", "connection"];
        // The default User-Agent names the build: CLIProxyAPI/dev in Go's test binary.
        let version = regex::Regex::new(r"^CLIProxyAPI/\S+").unwrap();
        let value = |v: &str| normalize(&version.replace(v, "CLIProxyAPI/<version>"));
        let mut want: BTreeMap<String, Vec<String>> = BTreeMap::new();
        for (k, v) in header_pairs(&go["headers"]) {
            want.entry(k.to_lowercase()).or_default().push(value(&v));
        }
        let mut got: BTreeMap<String, Vec<String>> = BTreeMap::new();
        for (k, v) in &rust.headers {
            let k = k.to_lowercase();
            if want.contains_key(&k) || !transport.contains(&k.as_str()) {
                got.entry(k).or_default().push(value(v));
            }
        }
        for random in ["x-client-request-id"] {
            if want.contains_key(random) && got.contains_key(random) {
                want.remove(random);
                got.remove(random);
            }
        }
        if got != want {
            let only_rust: Vec<_> = got.iter().filter(|(k, v)| want.get(*k) != Some(v)).collect();
            let only_go: Vec<_> = want.iter().filter(|(k, v)| got.get(*k) != Some(v)).collect();
            diffs.push(format!(
                "exchange {i} headers:\n   rust: {only_rust:?}\n     go: {only_go:?}"
            ));
        }
        let (rust_body, go_body) = (normalize(&text(&rust.body)), normalize(&text(&bytes(&go["body"]))));
        if rust_body != go_body {
            diffs.push(format!("exchange {i} body:\n   rust: {rust_body}\n     go: {go_body}"));
        }
    }
    if diffs.is_empty() {
        Ok(())
    } else {
        Err(diffs.join("\n"))
    }
}
