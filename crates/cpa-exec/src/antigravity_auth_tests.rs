//! Antigravity auth against `tests/fixtures/antigravity_go.json`: Go's
//! `CompleteAntigravityOAuth`, `PrepareRequestAuth` and `Refresh` answered by scripted
//! replies (tests/reference/antigravity). The Rust calls go to one local TLS server that
//! every Google host resolves to and that answers the same replies; each request must
//! carry what Go handed its transport, and the outcome must be Go's.
//!
//! Go's internal/auth/antigravity auth_test.go cases are among them, with the same
//! requests and outcomes: TestFetchProjectIDFromLoadCodeAssist is `complete_basic` (the
//! loadCodeAssist request and its project), TestFetchProjectIDFallsBackToDailyOnboardUser
//! is `complete_onboard_default_tier` (onboardUser on the daily endpoint with the default
//! tier), and TestFetchProjectIDUpstreamForbiddenReturnsStatus403 is
//! `prepare_project_denied` (a 403 status error).
//!
//! So are the sdk/auth antigravity_headless_test.go cases on the login helpers:
//! TestAntigravityConstantsAndURLBuilder is the `auth_urls` cases (state, a custom
//! redirect, the code/offline/consent/cloud-platform query and the blank-redirect
//! fallback to http://localhost:51121/oauth-callback), TestExchangeAntigravityCode,
//! TestFetchAntigravityUserInfo, TestBuildAntigravityAuth and TestCompleteAntigravityOAuth
//! are `complete_basic` (the code exchange, userinfo and the saved record), and the
//! version tests' user agents cover TestAntigravityUserAgent.

use std::sync::{Arc, Mutex};

use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};

use super::*;
use crate::proxy::Hooks;

/// Recorded at 6fecc6e, whose fallback client version 8fb633c9 raised to 2.23.0.
fn fixture() -> Value {
    let raw = include_str!("../tests/fixtures/antigravity_go.json");
    serde_json::from_str(&raw.replace("2.9.1", crate::antigravity_version::FALLBACK_VERSION)).unwrap()
}

/// The test CA and `*.googleapis.com` leaf recorded with the Vertex goldens.
fn tls() -> Value {
    let vertex: Value = serde_json::from_str(include_str!("../tests/fixtures/vertex_go.json")).unwrap();
    vertex["tls"].clone()
}

const HOSTS: [&str; 4] = [
    "oauth2.googleapis.com",
    "www.googleapis.com",
    "cloudcode-pa.googleapis.com",
    "daily-cloudcode-pa.googleapis.com",
];

/// A TLS server answering `replies` in order (599 when they run out) on keep-alive
/// connections, recording each raw request.
struct Mock {
    addr: std::net::SocketAddr,
    requests: Arc<Mutex<Vec<String>>>,
}

impl Mock {
    async fn start(replies: Vec<(u16, String)>) -> Self {
        let tls = tls();
        let cert = btls::x509::X509::from_pem(tls["leaf_pem"].as_str().unwrap().as_bytes()).unwrap();
        let key = btls::pkey::PKey::private_key_from_pem(tls["leaf_key_pem"].as_str().unwrap().as_bytes()).unwrap();
        let mut builder = btls::ssl::SslAcceptor::mozilla_intermediate_v5(btls::ssl::SslMethod::tls()).unwrap();
        builder.set_certificate(&cert).unwrap();
        builder.set_private_key(&key).unwrap();
        let acceptor = Arc::new(builder.build());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let replies = Arc::new(Mutex::new(std::collections::VecDeque::from(replies)));
        let log = requests.clone();
        tokio::spawn(async move {
            while let Ok((socket, _)) = listener.accept().await {
                let (acceptor, log, replies) = (acceptor.clone(), log.clone(), replies.clone());
                tokio::spawn(async move {
                    let ssl = btls::ssl::Ssl::new(acceptor.context()).unwrap();
                    let mut stream = tokio_btls::SslStream::new(ssl, socket).unwrap();
                    if std::pin::Pin::new(&mut stream).accept().await.is_err() {
                        return;
                    }
                    let mut reader = BufReader::new(stream);
                    loop {
                        let mut raw = String::new();
                        let mut length = 0usize;
                        loop {
                            let mut line = String::new();
                            if reader.read_line(&mut line).await.unwrap_or(0) == 0 {
                                return;
                            }
                            if let Some(v) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                                length = v.trim().parse().unwrap_or(0);
                            }
                            raw.push_str(&line);
                            if line == "\r\n" {
                                break;
                            }
                        }
                        let mut body = vec![0; length];
                        reader.read_exact(&mut body).await.unwrap();
                        raw.push_str(&String::from_utf8_lossy(&body));
                        log.lock().unwrap().push(raw);
                        let (status, body) = replies
                            .lock()
                            .unwrap()
                            .pop_front()
                            .unwrap_or((599, "unscripted".into()));
                        let reply = format!(
                            "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
                            body.len()
                        );
                        if reader.get_mut().write_all(reply.as_bytes()).await.is_err() {
                            return;
                        }
                    }
                });
            }
        });
        Self { addr, requests }
    }

    fn clients(&self) -> GoClients {
        let ca = tls()["ca_pem"].as_str().unwrap().to_owned();
        let trust = wreq::tls::trust::CertStore::from_pem_certs([ca.as_bytes()]).unwrap();
        GoClients::new(Hooks {
            trust: Some(trust),
            resolve: HOSTS.iter().map(|h| ((*h).to_owned(), self.addr)).collect(),
        })
        .http1_only()
    }
}

/// A wire request matches what Go handed its transport: method, target, Host, body,
/// and the header map plus only what a transport adds.
fn check_request(name: &str, wire: &str, go: &Value) {
    let (head, body) = wire.split_once("\r\n\r\n").unwrap();
    let mut lines = head.lines();
    let mut request_line = lines.next().unwrap().split(' ');
    let (method, target) = (request_line.next().unwrap(), request_line.next().unwrap());
    let url = url::Url::parse(go["url"].as_str().unwrap()).unwrap();
    let want_target = match url.query() {
        Some(q) => format!("{}?{q}", url.path()),
        None => url.path().to_owned(),
    };
    assert_eq!(
        (method, target),
        (go["method"].as_str().unwrap(), want_target.as_str()),
        "{name}"
    );
    assert_eq!(body, go["body"].as_str().unwrap(), "{name}: body");
    let wire_headers: Vec<(String, String)> = lines
        .filter_map(|l| l.split_once(": "))
        .map(|(n, v)| (n.to_ascii_lowercase(), v.to_owned()))
        .collect();
    let host = wire_headers.iter().find(|(n, _)| n == "host").map(|(_, v)| v.as_str());
    assert_eq!(host, url.host_str(), "{name}: host");
    let go_headers: Vec<(String, String)> = go["headers"]
        .as_array()
        .unwrap()
        .iter()
        .map(|h| {
            (
                h[0].as_str().unwrap().to_ascii_lowercase(),
                h[1].as_str().unwrap().to_owned(),
            )
        })
        .collect();
    for header in &go_headers {
        assert!(wire_headers.contains(header), "{name}: Go sent {header:?}");
    }
    for (n, v) in &wire_headers {
        let from_go = go_headers.iter().any(|(gn, gv)| gn == n && gv == v);
        assert!(
            from_go || matches!(n.as_str(), "host" | "user-agent" | "accept-encoding" | "content-length"),
            "{name}: extra {n}: {v}"
        );
    }
}

/// Go's recorded metadata view: clock fields as placeholders.
fn stable(metadata: &Map<String, Value>) -> Value {
    Value::Object(
        metadata
            .iter()
            .map(|(k, v)| {
                let v = if k == "timestamp" || k == "expired" {
                    json!("<clock>")
                } else {
                    v.clone()
                };
                (k.clone(), v)
            })
            .collect(),
    )
}

fn credential(case: &Value) -> Credential {
    let mut metadata = case["metadata"].as_object().unwrap().clone();
    let at =
        |delta: chrono::TimeDelta| (chrono::Local::now() + delta).to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    match metadata.get("expired").and_then(Value::as_str) {
        Some("FAR") => metadata.insert("expired".into(), json!(at(chrono::TimeDelta::hours(24)))),
        Some("PAST") => metadata.insert("expired".into(), json!(at(-chrono::TimeDelta::hours(1)))),
        _ => None,
    };
    let mut c = Credential::from_file(Path::new("/fixture"), Path::new("/fixture/antigravity.json"), metadata).unwrap();
    // Never an environment proxy: the Google hosts resolve to the local mock.
    c.attributes.insert("proxy_url".into(), "direct".into());
    c
}

/// Go's error text for `case`, with the token endpoint's body withheld: only its OAuth
/// error code (or Google API error status) is kept (docs/DIFFERENCES-FROM-GO.md).
fn go_error(case: &Value) -> Option<String> {
    let go = case["error"].as_str()?;
    let token_reply = |status: u16| {
        case["replies"]
            .as_array()
            .into_iter()
            .flatten()
            .find(|r| r["status"].as_u64() == Some(u64::from(status)))
            .and_then(|r| r["body"].as_str())
            .unwrap_or_default()
            .to_owned()
    };
    let code = |body: &str| {
        let error = gj::get(body.as_bytes(), "error");
        if error.kind == gj::Kind::String {
            error.str().into_owned()
        } else {
            error.get("status").str().into_owned()
        }
    };
    const EXCHANGE: &str = "antigravity: token exchange failed: antigravity token exchange: request failed: status ";
    if let Some(rest) = go.strip_prefix(EXCHANGE)
        && let Some((status, body)) = rest.split_once(": ")
    {
        let code = code(body);
        let suffix = if code.is_empty() {
            String::new()
        } else {
            format!(": {code}")
        };
        return Some(format!("{EXCHANGE}{status}{suffix}"));
    }
    // A failed refresh grant: Go's error is the body itself.
    if let Some(status) = case["status"].as_u64().map(|s| s as u16)
        && case["op"] != "complete"
        && go == token_reply(status)
        && !go.is_empty()
    {
        let code = code(go);
        let suffix = if code.is_empty() {
            String::new()
        } else {
            format!(": {code}")
        };
        return Some(format!("token refresh failed with status {status}{suffix}"));
    }
    Some(go.to_owned())
}

#[tokio::test]
async fn auth_matches_go() {
    let fixture = fixture();
    let cases = fixture["auth"].as_array().unwrap();
    assert!(cases.len() >= 30);
    let cfg = Config::parse("{}\n").unwrap();
    for case in cases {
        let name = case["name"].as_str().unwrap();
        let replies = case["replies"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|r| {
                (
                    r["status"].as_u64().unwrap() as u16,
                    r["body"].as_str().unwrap().to_owned(),
                )
            })
            .collect();
        let mock = Mock::start(replies).await;
        let clients = mock.clients();
        match case["op"].as_str().unwrap() {
            "complete" => {
                let oauth = OAuth::new(clients.get(&Proxy::Direct)).with_poll_delay(Duration::ZERO);
                let got = complete(
                    &oauth,
                    "4/fake-code",
                    "http://localhost:51121/oauth-callback",
                    Utc::now(),
                )
                .await;
                match go_error(case) {
                    Some(error) => assert_eq!(got.map(|_| ()).map_err(|e| e.message), Err(error), "{name}"),
                    None => {
                        let (file, metadata) = got.unwrap_or_else(|e| panic!("{name}: {e}"));
                        assert_eq!(file, case["file_name"].as_str().unwrap(), "{name}: file");
                        assert_eq!(stable(&metadata), case["result"], "{name}: metadata");
                    }
                }
            }
            op => {
                let credential = credential(case);
                if case["unchanged"] == true {
                    assert!(!must_prepare(&credential), "{name}: Go prepares nothing");
                } else {
                    assert!(op == "refresh" || must_prepare(&credential), "{name}");
                    let got = prepare(&clients, &credential, &cfg).await;
                    match go_error(case) {
                        Some(error) => {
                            let e = got.err().unwrap_or_else(|| panic!("{name}: no error"));
                            assert_eq!(String::from_utf8_lossy(&e.body), error, "{name}: error");
                            if let Some(status) = case["status"].as_u64() {
                                assert_eq!(u64::from(e.status), status, "{name}: status");
                            }
                            assert_eq!(
                                e.retry_after.map(|d| d.as_secs_f64()),
                                case["retry_after"].as_f64(),
                                "{name}: retry"
                            );
                        }
                        None => {
                            let patch = got.unwrap_or_else(|e| panic!("{name}: {e:?}"));
                            let mut metadata = credential.metadata.clone();
                            patch.apply(&mut metadata);
                            assert_eq!(stable(&metadata), case["result"], "{name}: metadata");
                        }
                    }
                }
            }
        }
        let wire = mock.requests.lock().unwrap().clone();
        // Go marshals an empty request list as null.
        let go = case["requests"].as_array().cloned().unwrap_or_default();
        assert_eq!(wire.len(), go.len(), "{name}: requests {wire:#?}");
        for (wire, go) in wire.iter().zip(&go) {
            check_request(name, wire, go);
        }
    }
}

/// `BuildAntigravityAuthURL` as Go built it.
#[test]
fn auth_url_matches_go() {
    let fixture = fixture();
    let urls = fixture["auth_urls"].as_object().unwrap();
    assert!(urls.len() >= 3);
    for (input, want) in urls {
        let (state, redirect) = input.split_once('|').unwrap();
        assert_eq!(auth_url(state, redirect), want.as_str().unwrap(), "{input}");
    }
    assert_eq!(credential_file_name(" a@b.c "), "antigravity-a@b.c.json");
    assert_eq!(credential_file_name(""), "antigravity.json");
}

#[test]
fn hide_follows_go_bytes() {
    assert_eq!(hide("proj-123456"), "proj...3456");
    assert_eq!(hide("abcdef"), "ab...ef");
    assert_eq!(hide("abc"), "a...c");
    assert_eq!(hide("ab"), "ab");
}

/// `helps.ParseRetryDelay` on recorded inputs, with Go's results
/// (TestParseRetryDelay_HumanReadableDuration is the first). Negative Go durations
/// clamp to zero.
#[test]
fn retry_delays_match_go() {
    let cases: [(&str, Option<u64>); 12] = [
        (
            r#"{"error":{"message":"You have exhausted your capacity on this model. Your quota will reset after 1h43m56s."}}"#,
            Some(6236000000000),
        ),
        (r#"{"error":{"message":"Please retry after 4s."}}"#, Some(4000000000)),
        (r#"{"error":{"message":"Retry AFTER 2M."}}"#, Some(120000000000)),
        (
            r#"{"error":{"message":"retry after 99999999999999999999s or after 3m"}}"#,
            None,
        ),
        (r#"{"error":{"message":"after the break, after 5m"}}"#, None),
        (r#"{"error":{"message":"after 0s"}}"#, Some(0)),
        (r#"{"error":{"message":"after 10000000000s"}}"#, Some(0)),
        (
            r#"{"error":{"details":[{"@type":"type.googleapis.com/google.rpc.RetryInfo","retryDelay":"1.5s"}],"message":"after 9s"}}"#,
            Some(1500000000),
        ),
        (
            r#"{"error":{"details":[{"@type":"type.googleapis.com/google.rpc.RetryInfo","retryDelay":"bogus"}],"message":"after 9s"}}"#,
            None,
        ),
        (
            r#"{"error":{"details":[{"@type":"type.googleapis.com/google.rpc.ErrorInfo","metadata":{"quotaResetDelay":"bad"}},{"@type":"type.googleapis.com/google.rpc.ErrorInfo","metadata":{"quotaResetDelay":"-2s"}}]}}"#,
            Some(0),
        ),
        (
            r#"{"error":{"details":[{"@type":"type.googleapis.com/google.rpc.RetryInfo","retryDelay":"-3s"}]}}"#,
            Some(0),
        ),
        (r#"{"error":{"message":""}}"#, None),
    ];
    for (body, want) in cases {
        let got = parse_retry_delay(body.as_bytes()).map(|d| d.as_nanos() as u64);
        assert_eq!(got, want, "{body}");
    }
}
