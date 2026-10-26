//! Differential tests against `tests/fixtures/vertex_go.json`, produced by the real Go
//! Vertex executor, key normalization and `-vertex-import` (tests/reference/vertex).
//! Google hosts are reached through a local CONNECT proxy that terminates TLS with the
//! fixture's test CA, as in the Go generator. Expected values come only from Go.

use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex};

use base64::Engine;
use cpa_core::exec::Caller;
use futures_util::StreamExt;
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader};

use super::*;

const FIXTURE: &str = include_str!("../tests/fixtures/vertex_go.json");

/// `vertex_go.json` with the API-key scenarios recorded again (or added) at v8.0.35.
fn fixture() -> &'static Value {
    static PARSED: std::sync::OnceLock<Value> = std::sync::OnceLock::new();
    PARSED.get_or_init(|| {
        crate::payload_barrier::with_v8_0_35(
            FIXTURE,
            include_str!("../tests/fixtures/vertex_go_v8.0.35.json"),
            "scenarios",
        )
    })
}

/// The JWT clock for the scenario running (Go's recorded `iat` + 10 s).
static NOW: AtomicI64 = AtomicI64::new(0);

fn now() -> i64 {
    NOW.load(Ordering::SeqCst)
}

fn acceptor() -> btls::ssl::SslAcceptor {
    let tls = &fixture()["tls"];
    let cert = btls::x509::X509::from_pem(tls["leaf_pem"].as_str().unwrap().as_bytes()).unwrap();
    let key = btls::pkey::PKey::private_key_from_pem(tls["leaf_key_pem"].as_str().unwrap().as_bytes()).unwrap();
    let mut builder = btls::ssl::SslAcceptor::mozilla_intermediate_v5(btls::ssl::SslMethod::tls()).unwrap();
    builder.set_certificate(&cert).unwrap();
    builder.set_private_key(&key).unwrap();
    builder.build()
}

async fn read_request<S: AsyncRead + Unpin>(reader: &mut BufReader<S>) -> Vec<u8> {
    let mut raw = Vec::new();
    let mut length = 0;
    loop {
        let mut line = Vec::new();
        if reader.read_until(b'\n', &mut line).await.unwrap_or(0) == 0 {
            return raw;
        }
        raw.extend_from_slice(&line);
        if let Some(v) = String::from_utf8_lossy(&line)
            .to_ascii_lowercase()
            .strip_prefix("content-length:")
        {
            length = v.trim().parse().unwrap_or(0);
        }
        if line == b"\r\n" {
            break;
        }
    }
    let mut body = vec![0; length];
    reader.read_exact(&mut body).await.unwrap();
    raw.extend_from_slice(&body);
    raw
}

async fn answer<S: AsyncRead + AsyncWrite + Unpin>(stream: S, reply: &Value, sink: &Mutex<Vec<Vec<u8>>>) {
    let mut reader = BufReader::new(stream);
    let raw = read_request(&mut reader).await;
    sink.lock().unwrap().push(raw);
    let status = reply["status"].as_u64().unwrap() as u16;
    let reason = http::StatusCode::from_u16(status)
        .ok()
        .and_then(|s| s.canonical_reason())
        .unwrap_or("");
    let body = reply["body"].as_str().unwrap();
    let mut out = format!("HTTP/1.1 {status} {reason}\r\n");
    for h in reply["headers"].as_array().into_iter().flatten() {
        out.push_str(&format!("{}: {}\r\n", h[0].as_str().unwrap(), h[1].as_str().unwrap()));
    }
    // `missing`: bytes promised beyond the body, a connection closed mid-stream.
    let missing = reply["missing"].as_u64().unwrap_or(0) as usize;
    out.push_str(&format!(
        "Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len() + missing
    ));
    let _ = reader.get_mut().write_all(out.as_bytes()).await;
    if reply["stall"] == true {
        // `stall`: the connection stays open until the client closes it.
        let mut rest = Vec::new();
        let _ = tokio::time::timeout(std::time::Duration::from_secs(5), reader.read_to_end(&mut rest)).await;
    }
    let _ = reader.get_mut().shutdown().await;
}

/// Answers connections in order with the scripted replies; with `tls`, each connection
/// is a CONNECT tunnel that is then TLS-terminated.
struct Capture {
    addr: String,
    requests: Arc<Mutex<Vec<Vec<u8>>>>,
    task: tokio::task::JoinHandle<()>,
}

impl Capture {
    async fn start(replies: Vec<Value>, tls: bool) -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        let requests: Arc<Mutex<Vec<Vec<u8>>>> = Arc::default();
        let sink = requests.clone();
        let acceptor = tls.then(acceptor);
        let task = tokio::spawn(async move {
            for reply in replies {
                let Ok((mut socket, _)) = listener.accept().await else {
                    return;
                };
                match &acceptor {
                    None => answer(socket, &reply, &sink).await,
                    Some(acceptor) => {
                        let mut head = BufReader::new(&mut socket);
                        read_request(&mut head).await;
                        socket.write_all(b"HTTP/1.1 200 OK\r\n\r\n").await.unwrap();
                        let ssl = btls::ssl::Ssl::new(acceptor.context()).unwrap();
                        let mut stream = tokio_btls::SslStream::new(ssl, socket).unwrap();
                        std::pin::Pin::new(&mut stream).accept().await.unwrap();
                        answer(stream, &reply, &sink).await;
                    }
                }
            }
        });
        Self { addr, requests, task }
    }
}

fn credential(s: &Value, cfg: &Config, plain: &str, proxy: &str) -> Credential {
    let index = s["config_auth"].as_i64().unwrap();
    if index >= 0 {
        let vertex: Vec<Credential> = cpa_core::config::credentials::from_config(cfg)
            .into_iter()
            .filter(|c| c.provider == "vertex")
            .collect();
        return vertex[index as usize].clone();
    }
    let text = serde_json::to_string(&s["metadata"])
        .unwrap()
        .replace("PROXY", proxy)
        .replace("UPSTREAM", plain);
    let mut metadata: serde_json::Map<String, Value> = serde_json::from_str(&text).unwrap();
    if let (Some(Value::Object(sa)), Some(key)) = (metadata.get_mut("service_account"), s["key"].as_str()) {
        sa.insert("private_key".into(), fixture()["keys"][key].clone());
    }
    Credential::from_file(
        std::path::Path::new("/fixture"),
        std::path::Path::new("/fixture/vertex-test.json"),
        metadata,
    )
    .unwrap()
}

fn request(s: &Value) -> ExecRequest {
    let op = s["op"].as_str().unwrap();
    let source = Format::parse(s["source"].as_str().unwrap()).unwrap();
    let model = s["model"].as_str().unwrap().to_owned();
    let body = Bytes::from(s["payload"].as_str().unwrap().to_owned());
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
        response_format: source,
        stream_payloads: false,
        requested_model: s["requested_model"].as_str().unwrap_or(&model).to_owned(),
        model,
        original_body: body.clone(),
        body,
        stream: op == "stream",
        alt: s["alt"].as_str().map(str::to_owned),
        session: s["session"].as_str().map(str::to_owned),
        execution_session: None,
        derived_session: None,
        request_path: String::new(),
        resolved_model: crate::gemini::tests::resolved_model(s),
        usage: Default::default(),
        headers,
        caller: Caller {
            principal: "fake-client-key".into(),
            source: "authorization",
            provider: String::new(),
        },
    }
}

/// Go's `iat` from the recorded token exchange, if any.
fn recorded_iat(s: &Value) -> Option<i64> {
    let first = s["requests"].as_array()?.first()?.as_str()?;
    let body = first.split("\r\n\r\n").nth(1)?;
    let assertion = body.strip_prefix("assertion=")?.split('&').next()?;
    let claims = assertion.split('.').nth(1)?;
    let claims = base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(claims).ok()?;
    serde_json::from_slice::<Value>(&claims).ok()?["iat"].as_i64()
}

#[tokio::test]
async fn go_reference_scenarios() {
    let scenarios = fixture()["scenarios"].as_array().unwrap();
    assert!(scenarios.len() >= 40, "fixture lost scenarios");
    let ca = fixture()["tls"]["ca_pem"].as_str().unwrap().as_bytes().to_vec();
    let trust = wreq::tls::trust::CertStore::from_pem_certs([ca.as_slice()]).unwrap();
    let executor = VertexExecutor::with_hooks(
        Hooks {
            trust: Some(trust),
            resolve: vec![],
        },
        now,
    );
    let imagen_id = regex::Regex::new(r#""responseId":"imagen-[0-9]+""#).unwrap();
    // Responses output stamps `created_at` and mints call IDs from the wall clock
    // (`call_<UnixNano, hex or decimal>_<n>`); only their shape can match a fixture.
    let created = regex::Regex::new(r#""created_at":1[0-9]{9}([,}])"#).unwrap();
    // Responses call IDs: `call_<UnixNano>_<n>`, where n is a process-wide counter that
    // other executors' tests in this binary advance too.
    let call_ids = regex::Regex::new(r"call_[0-9a-f]{16,20}_[0-9]+").unwrap();
    for s in scenarios {
        let name = s["name"].as_str().unwrap();
        let replies: Vec<Value> = s["replies"].as_array().cloned().unwrap_or_default();
        let plain_replies = if s["via"] == "plain" { replies.clone() } else { vec![] };
        let proxy_replies = if s["via"] == "plain" { vec![] } else { replies };
        let plain = Capture::start(plain_replies, false).await;
        let proxy = Capture::start(proxy_replies, true).await;
        let config = s["config"]
            .as_str()
            .unwrap_or_default()
            .replace("UPSTREAM", &plain.addr)
            .replace("PROXY", &proxy.addr);
        let cfg = Config::parse(&config).unwrap();
        let cred = credential(s, &cfg, &plain.addr, &proxy.addr);
        if let Some(iat) = recorded_iat(s) {
            NOW.store(iat + 10, Ordering::SeqCst);
        }
        let mut req = request(s);
        let reports = std::sync::Arc::new(crate::gemini::tests::UsageReports::default());
        let captures = std::sync::Arc::new(crate::gemini::tests::Captures::default());
        req.usage = reports.sink().with_capture(captures.sink());
        let (client, alt) = (req.response_format, req.alt.as_deref().is_some_and(|a| !a.is_empty()));
        let mut output = None;
        let mut streamed = Vec::new();
        let mut error = None;
        let mut mid_stream = false;
        match executor.execute(&cred, req, &cfg).await {
            Err(e) => error = Some((e.status, String::from_utf8_lossy(&e.body).into_owned())),
            Ok(response) => match response.body {
                ResponseBody::Buffered(bytes) => output = Some(String::from_utf8(bytes.to_vec()).unwrap()),
                ResponseBody::Stream(mut stream) => {
                    mid_stream = true;
                    loop {
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
                            Some(Err(e)) => error = Some((e.status, String::from_utf8_lossy(&e.body).into_owned())),
                            None => break,
                        }
                    }
                }
            },
        }
        // A Go error without a status code is answered 500 by Go's handler. Mid-stream
        // (a scanner error after the status line) no status reaches the client, so only
        // the message is compared.
        let want_error = s["error"].as_object().map(|e| {
            let status = e["status"].as_u64().unwrap() as u16;
            (
                if status == 0 { 500 } else { status },
                e["message"].as_str().unwrap().to_owned(),
            )
        });
        // A statusless Rust error is rendered 500 too.
        let error = error.map(|(status, message)| (if status == 0 { 500 } else { status }, message));
        let error = error.map(|(status, message)| match want_error {
            Some((500, _)) if mid_stream && s["error"]["status"] == 0 => (500, message),
            _ => (status, message),
        });
        assert_eq!(error, want_error, "{name}: error");
        let norm = |t: &str| {
            let t = imagen_id.replace_all(t, r#""responseId":"imagen-<nanos>""#);
            let t = created.replace_all(&t, r#""created_at":<unix>$1"#);
            call_ids.replace_all(&t, "call_<nanos>_<n>").into_owned()
        };
        assert_eq!(
            output.as_deref().map(norm),
            s["output"].as_str().map(norm),
            "{name}: output"
        );
        let want_stream =
            crate::gemini::tests::client_bytes(client, alt, s["chunks"].as_array().map_or(&[][..], Vec::as_slice));
        assert_eq!(
            norm(&String::from_utf8_lossy(&streamed)),
            norm(&String::from_utf8_lossy(&want_stream)),
            "{name}: stream"
        );
        let mut got: Vec<String> = Vec::new();
        for capture in [&plain, &proxy] {
            for raw in capture.requests.lock().unwrap().iter() {
                got.push(
                    String::from_utf8(raw.clone())
                        .unwrap()
                        .replace(&plain.addr, "UPSTREAM")
                        .replace(&proxy.addr, "PROXY"),
                );
            }
        }
        let want: Vec<String> = s["requests"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|r| r.as_str().unwrap().to_owned())
            .collect();
        assert_eq!(got, want, "{name}: upstream requests");
        reports.check(s);
        // The record's identity (recorded since d9451cb6): the Vertex executor and the
        // requested base model unless the attempt was handed to another executor.
        if let Some(executor_type) = s["usage"]["executor_type"].as_str() {
            let base = cpa_common::thinking::parse_suffix(s["model"].as_str().unwrap()).model_name;
            let identity = reports
                .delegated
                .lock()
                .unwrap()
                .clone()
                .unwrap_or_else(|| ("GeminiVertexExecutor".into(), base));
            let want = (
                executor_type.to_owned(),
                s["usage"]["model"].as_str().unwrap().to_owned(),
            );
            assert_eq!(identity, want, "{name}: usage identity");
        }
        // Every model request is captured with its reply; the token exchange is not
        // (Go logs only the executor's own request).
        let mut events: std::collections::VecDeque<_> = captures.events().into();
        let replies = s["replies"].as_array().cloned().unwrap_or_default();
        for (i, wire) in want.iter().enumerate() {
            let target = wire.split(' ').nth(1).unwrap_or_default();
            if target.split('?').next().unwrap_or_default().ends_with("/token") {
                continue;
            }
            // Go AccountInfo: config keys are API-key accounts, file credentials OAuth ones
            // named by their email. The endpoints/openapi route sends the key as a bearer.
            let key_value = wire
                .lines()
                .find_map(|l| {
                    l.strip_prefix("X-Goog-Api-Key: ")
                        .or_else(|| l.strip_prefix("Authorization: Bearer "))
                })
                .unwrap_or_default();
            let email = s["metadata"]["email"].as_str().unwrap_or_default();
            let account = if s["config_auth"].as_i64().unwrap() >= 0 {
                ("api_key", key_value)
            } else {
                ("oauth", email)
            };
            crate::gemini::tests::check_attempt(
                name,
                &mut events,
                wire,
                replies.get(i),
                s["op"] == "stream",
                "vertex",
                Some(account),
            );
        }
        crate::gemini::tests::check_refused_capture(name, s, &mut events);
        assert!(events.is_empty(), "{name}: extra captures {events:?}");
        plain.task.abort();
        proxy.task.abort();
    }
}

#[test]
fn private_key_normalization_matches_go() {
    let cases = fixture()["normalize"].as_array().unwrap();
    assert!(cases.len() >= 10);
    for case in cases {
        let name = case["key"].as_str().unwrap();
        let mut sa = serde_json::Map::new();
        sa.insert("private_key".into(), fixture()["keys"][name].clone());
        let got = vertex_auth::normalize_service_account(&sa).map(|m| m["private_key"].as_str().unwrap().to_owned());
        match case["error"].as_str() {
            Some(error) => assert_eq!(got, Err(error.to_owned()), "{name}"),
            None => assert_eq!(got.as_deref(), Ok(case["out"].as_str().unwrap()), "{name}"),
        }
    }
}

/// `-vertex-import` writes Go's file, name and content, prints Go's (cleaned) path, and
/// fails with Go's logged error.
#[test]
fn vertex_import_matches_go() {
    let cases = fixture()["import"].as_array().unwrap();
    assert!(cases.len() >= 16);
    for (i, case) in cases.iter().enumerate() {
        let name = case["name"].as_str().unwrap();
        let dir = std::env::temp_dir().join(format!("cpa-vertex-import-{}-{i}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let key = dir.join("key.json");
        if case["no_key_file"].as_bool() != Some(true) {
            std::fs::write(&key, case["input"].as_str().unwrap()).unwrap();
        }
        std::fs::write(dir.join("blocked"), b"").unwrap();
        // Joined by hand, as Go's generator does: the import itself must clean it.
        let auth_dir = format!("{}/{}", dir.display(), case["auth_dir"].as_str().unwrap());
        let result = vertex_auth::import(
            std::path::Path::new(&auth_dir),
            key.to_str().unwrap(),
            case["prefix"].as_str().unwrap_or_default(),
        );
        // Files are listed where the import wrote (Go lists the cleaned auth dir).
        let written = result
            .as_ref()
            .ok()
            .and_then(|path| path.parent())
            .map_or_else(|| std::path::PathBuf::from(&auth_dir), std::path::Path::to_path_buf);
        let normalized = |text: &str| text.replace(&dir.display().to_string(), "DIR");
        let want = match case["errors"].as_array() {
            Some(errors) => {
                assert_eq!(errors.len(), 1, "{name}");
                Err(errors[0].as_str().unwrap().to_owned())
            }
            None => Ok(case["imported"].as_str().unwrap().to_owned()),
        };
        #[cfg(unix)]
        if let Ok(path) = &result {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(path).unwrap().permissions().mode() & 0o777,
                0o600,
                "{name}"
            );
        }
        let got = result
            .map(|path| normalized(&path.display().to_string()))
            .map_err(|e| normalized(&e));
        assert_eq!(got, want, "{name}");
        let mut files = serde_json::Map::new();
        for entry in std::fs::read_dir(&written).into_iter().flatten().flatten() {
            let content = std::fs::read_to_string(entry.path()).unwrap();
            files.insert(entry.file_name().to_string_lossy().into_owned(), Value::String(content));
        }
        assert_eq!(Value::Object(files), case["files"], "{name}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}

/// Go's `pem.Decode` edge cases: junk around the block, a repeated BEGIN before the END
/// that matches, and END trailers that do not match.
#[test]
fn pem_decode_follows_go_rules() {
    let body = base64::engine::general_purpose::STANDARD.encode(b"hello");
    let block = format!("-----BEGIN X-----\n{body}\n-----END X-----\n");
    let decoded = vertex_auth::pem_decode(format!("junk\n{block}tail").as_bytes()).unwrap();
    assert_eq!((decoded.kind.as_str(), decoded.bytes.as_slice()), ("X", &b"hello"[..]));
    let repeated = format!("-----BEGIN Y-----\n{block}");
    assert_eq!(vertex_auth::pem_decode(repeated.as_bytes()).unwrap().kind, "X");
    assert!(vertex_auth::pem_decode(format!("-----BEGIN X-----\n{body}\n-----END Y-----\n").as_bytes()).is_none());
    assert!(
        vertex_auth::pem_decode(format!("-----BEGIN X-----\n{body}\n-----END X----- trailing\n").as_bytes()).is_none()
    );
    // BEGIN must start a line.
    assert!(vertex_auth::pem_decode(format!("x-----BEGIN X-----\n{body}\n-----END X-----\n").as_bytes()).is_none());
    let empty = vertex_auth::pem_decode(b"-----BEGIN E-----\n-----END E-----\n").unwrap();
    assert!(empty.bytes.is_empty());
}

/// Go marshals the service account with sorted keys before decoding it, so of keys that
/// fold to the same field the last in sorted order wins, whatever the file order.
#[test]
fn key_file_folded_duplicates_follow_sorted_order() {
    let pem = fixture()["keys"]["pkcs1"].as_str().unwrap();
    let account = |entries: &[(&str, &str)]| {
        let mut sa = serde_json::Map::new();
        for (k, v) in entries {
            sa.insert((*k).into(), Value::String((*v).into()));
        }
        sa.insert("private_key".into(), Value::String(pem.into()));
        sa
    };
    // File order puts the bogus value last; sorted, "typE" < "type".
    let sa = account(&[("type", "service_account"), ("typE", "bogus")]);
    let token = vertex_auth::token_request(&sa, 1_700_000_000);
    assert!(token.is_ok(), "{token:?}");
    let sa = account(&[("typE", "service_account"), ("type", "bogus")]);
    let token = vertex_auth::token_request(&sa, 1_700_000_000);
    assert_eq!(token.err().as_deref(), Some(r#"unknown credential type: "bogus""#));
}

/// A proxy that refuses every request and counts the connections it saw.
async fn refusing_proxy() -> (String, std::sync::Arc<std::sync::atomic::AtomicUsize>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let hits = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let counter = hits.clone();
    tokio::spawn(async move {
        while let Ok((mut socket, _)) = listener.accept().await {
            counter.fetch_add(1, Ordering::SeqCst);
            let _ = socket
                .write_all(b"HTTP/1.1 502 Bad Gateway\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                .await;
        }
    });
    (url, hits)
}

/// TestVertexAccessTokenUsesCredentialProxyNotRequestProxy: the service-account token
/// exchange goes through the credential's proxy, not the global `proxy-url`. (Rust has
/// no request-scoped proxy override, the third proxy of Go's case.)
#[tokio::test]
async fn token_exchange_uses_the_credential_proxy() {
    let (credential_proxy, credential_hits) = refusing_proxy().await;
    let (global_proxy, global_hits) = refusing_proxy().await;
    let cfg = Config::parse(&format!("proxy-url: {global_proxy}\n")).unwrap();
    let s = fixture()["scenarios"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["name"] == "sa_generate_default_location")
        .unwrap();
    let cred = credential(s, &cfg, "127.0.0.1:1", credential_proxy.trim_start_matches("http://"));
    let executor = VertexExecutor::with_hooks(Hooks::default(), now);
    let sa = cred.metadata["service_account"].as_object().unwrap().clone();
    let result = vertex_auth::access_token(&executor.client(&cred, &cfg), &sa, now()).await;
    assert!(result.is_err(), "{result:?}");
    assert!(
        credential_hits.load(Ordering::SeqCst) > 0,
        "the credential proxy was not used"
    );
    assert_eq!(global_hits.load(Ordering::SeqCst), 0, "the global proxy was used");
}

/// TestStripVertexToolCallIDsReusesPayloadWithoutIDs and
/// TestStripVertexToolCallIDsRebuildsContentsOnce (helps/vertex_payload_helpers_test.go):
/// Responses call IDs leave function calls and responses, nested `id` fields and large
/// integers stay exact, and a payload without IDs is unchanged.
#[test]
fn responses_tool_call_ids_are_stripped_like_go() {
    let without = br#"{"contents":[{"role":"model","parts":[{"functionCall":{"name":"lookup","args":{"id":9007199254740993}}}]}]}"#;
    assert_eq!(
        strip_tool_call_ids(without.to_vec(), Format::OpenAIResponse),
        without.to_vec()
    );
    let with = br#"{"contents":[{"role":"model","parts":[{"functionCall":{"id":"call_1","name":"lookup","args":{"id":9007199254740993}}}]},{"role":"user","parts":[{"functionResponse":{"id":"call_1","name":"lookup","response":{"id":"keep"}}}]}]}"#;
    let out = strip_tool_call_ids(with.to_vec(), Format::OpenAIResponse);
    let get = |path: &str| cpa_common::json::get(&out, path);
    assert!(!get("contents.0.parts.0.functionCall.id").exists());
    assert!(!get("contents.1.parts.0.functionResponse.id").exists());
    assert_eq!(get("contents.1.parts.0.functionResponse.response.id").str(), "keep");
    assert_eq!(
        get("contents.0.parts.0.functionCall.args.id").raw(),
        b"9007199254740993"
    );
    // Other sources keep their IDs.
    assert_eq!(strip_tool_call_ids(with.to_vec(), Format::Gemini), with.to_vec());
}

/// Go translates the original (baseline) body without checking its error: only the
/// working body's refusal refuses the request.
#[test]
fn a_refused_baseline_alone_does_not_refuse_the_request() {
    let cfg = Config::parse("").unwrap();
    let s = serde_json::json!({"op": "execute", "source": "claude", "model": "gemini-2.5-pro", "payload": r#"{"model":"gemini-2.5-pro","max_tokens":16,"messages":[{"role":"user","content":[{"type":"text","text":"hi"}]}]}"#});
    let mut req = request(&s);
    req.original_body = Bytes::from_static(br#"{"model":"gemini-2.5-pro","max_tokens":16,"messages":[{"role":"user","content":[{"type":"container_upload","source":{"type":"file","file_id":"f1"}}]}]}"#);
    let (original, body) = VertexExecutor::default()
        .prepared_body(&req, &cfg, "gemini-2.5-pro")
        .unwrap();
    assert_eq!(gj::get(&body, "contents.0.parts.0.text").str(), "hi");
    assert_eq!(gj::get(&original, "contents.0.parts.0.text").str(), "hi");
    std::mem::swap(&mut req.body, &mut req.original_body);
    assert!(
        VertexExecutor::default()
            .prepared_body(&req, &cfg, "gemini-2.5-pro")
            .is_err()
    );
}
