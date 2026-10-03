//! Differential tests against `tests/fixtures/xai_auth_go.json`, produced by the real Go
//! login manager, FileTokenStore and XAIExecutor.Refresh (tests/reference/xai_auth).
//! The same scripted answers are replayed to the Rust port; requests, saved files,
//! refreshed metadata and error messages are compared. Expected values come only from Go.

use std::sync::{Arc, Mutex};

use serde_json::{Map, Value};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};

use super::*;

const FIXTURE: &str = include_str!("../tests/fixtures/xai_auth_go.json");

/// Cases whose Go error is an encoding/json syntax message, which is not ported (see
/// `go_unmarshal`): only the text before it is compared.
const SYNTAX_MESSAGE: &[&str] = &["discovery_not_json", "token_body_not_json"];

/// Go's error with the number it quotes in a type mismatch removed ("number 1.5 into"
/// becomes "number into"): Rust names only the kind, as response data never reaches
/// error text.
fn without_quoted_number(want: &str) -> String {
    const MARK: &str = "cannot unmarshal number ";
    match want
        .split_once(MARK)
        .and_then(|(head, rest)| Some((head, rest.split_once(" into ")?.1)))
    {
        Some((head, tail)) => format!("{head}cannot unmarshal number into {tail}"),
        None => want.to_owned(),
    }
}

#[derive(Clone)]
struct Reply {
    path: String,
    status: u16,
    body: String,
}

/// A raw HTTP/1.1 capture server: one request per connection, answered with the first
/// scripted reply for its path (Go generator `server`).
struct Mock {
    addr: String,
    requests: Arc<Mutex<Vec<String>>>,
}

impl Mock {
    async fn start(script: &Value) -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        let script: Vec<Reply> = script
            .as_array()
            .into_iter()
            .flatten()
            .map(|r| Reply {
                path: r["path"].as_str().unwrap().to_owned(),
                status: r["status"].as_u64().unwrap() as u16,
                body: r["body"].as_str().unwrap().to_owned(),
            })
            .collect();
        let script = Arc::new(Mutex::new(script));
        let requests: Arc<Mutex<Vec<String>>> = Arc::default();
        let sink = requests.clone();
        let host = addr.clone();
        tokio::spawn(async move {
            while let Ok((socket, _)) = listener.accept().await {
                let (script, sink, host) = (script.clone(), sink.clone(), host.clone());
                tokio::spawn(async move {
                    let (read, mut write) = socket.into_split();
                    let mut reader = BufReader::new(read);
                    let mut raw = Vec::new();
                    let (mut length, mut path) = (0usize, String::new());
                    loop {
                        let mut line = Vec::new();
                        if reader.read_until(b'\n', &mut line).await.unwrap_or(0) == 0 {
                            return;
                        }
                        raw.extend_from_slice(&line);
                        let text = String::from_utf8_lossy(&line).into_owned();
                        if path.is_empty() {
                            path = text.split_whitespace().nth(1).unwrap_or_default().to_owned();
                        }
                        if let Some(v) = text.to_ascii_lowercase().strip_prefix("content-length:") {
                            length = v.trim().parse().unwrap_or(0);
                        }
                        if line == b"\r\n" {
                            break;
                        }
                    }
                    let mut body = vec![0; length];
                    reader.read_exact(&mut body).await.unwrap();
                    raw.extend_from_slice(&body);
                    sink.lock()
                        .unwrap()
                        .push(String::from_utf8_lossy(&raw).replace(&host, "UPSTREAM"));
                    let reply = {
                        let mut script = script.lock().unwrap();
                        match script.iter().position(|r| r.path == path) {
                            Some(i) => script.remove(i),
                            None => Reply {
                                path,
                                status: 599,
                                body: "unscripted".into(),
                            },
                        }
                    };
                    let reason = http::StatusCode::from_u16(reply.status)
                        .ok()
                        .and_then(|s| s.canonical_reason())
                        .unwrap_or("");
                    let out = format!(
                        "HTTP/1.1 {} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                        reply.status,
                        reply.body.len(),
                        reply.body
                    );
                    let _ = write.write_all(out.as_bytes()).await;
                    let _ = write.shutdown().await;
                });
            }
        });
        Self { addr, requests }
    }

    fn requests(&self) -> Vec<String> {
        self.requests.lock().unwrap().clone()
    }

    fn auth(&self) -> XaiAuth {
        XaiAuth::new(crate::proxy::default_client())
            .with_issuer_origin(&format!("http://{}", self.addr))
            .with_min_poll_interval(Duration::from_millis(1))
    }
}

/// Go's fixture replaces RFC 3339 UTC timestamps with `TIME`.
fn normalize_times(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while i < b.len() {
        let candidate = &b[i..(i + 20).min(b.len())];
        let shape = b"dddd-dd-ddTdd:dd:ddZ";
        if candidate.len() == 20
            && candidate
                .iter()
                .zip(shape)
                .all(|(c, p)| if *p == b'd' { c.is_ascii_digit() } else { c == p })
        {
            out.push_str("TIME");
            i += 20;
        } else {
            out.push(b[i] as char);
            i += 1;
        }
    }
    out
}

fn fixture() -> Value {
    serde_json::from_str(FIXTURE).unwrap()
}

#[test]
fn endpoint_validation_matches_go_net_url() {
    let f = fixture();
    let cases = f["validate"].as_array().unwrap();
    assert!(cases.len() >= 55, "fixture lost validation vectors");
    for case in cases {
        let raw = case[0].as_str().unwrap();
        let got = validate_endpoint(raw, "token_endpoint");
        let want = if case[1].as_bool().unwrap() {
            Ok(case[2].as_str().unwrap().to_owned())
        } else {
            Err(case[2].as_str().unwrap().to_owned())
        };
        assert_eq!(got, want, "ValidateOAuthEndpoint({raw:?})");
    }
}

#[test]
fn credential_file_names_match_go() {
    for case in fixture()["file_names"].as_array().unwrap() {
        let (email, subject, want) = (
            case[0].as_str().unwrap(),
            case[1].as_str().unwrap(),
            case[2].as_str().unwrap(),
        );
        assert_eq!(credential_file_name(email, subject, 0), want, "{email:?} {subject:?}");
    }
}

#[tokio::test]
async fn login_matches_go_manager_and_file_store() {
    let f = fixture();
    let cases = f["login"].as_array().unwrap();
    assert!(cases.len() >= 22, "fixture lost login cases");
    for case in cases {
        let name = case["name"].as_str().unwrap();
        let mock = Mock::start(&case["script"]).await;
        let dir = std::env::temp_dir().join(format!("xai-login-{name}-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        if let Some(existing) = case["existing"].as_str() {
            std::fs::write(dir.join(case["existing_name"].as_str().unwrap()), existing).unwrap();
        }
        let result = login_with(mock.auth(), &dir, true).await;
        let want_requests: Vec<String> = case["requests"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| r.as_str().unwrap().to_owned())
            .collect();
        assert_eq!(mock.requests(), want_requests, "{name}: requests");
        match result {
            Ok(path) => {
                assert!(case["error"].is_null(), "{name}: Go failed with {}", case["error"]);
                let file_name = path.file_name().unwrap().to_str().unwrap().to_owned();
                let want_name = case["file_name"].as_str().unwrap();
                if want_name
                    .trim_start_matches("xai-")
                    .trim_end_matches(".json")
                    .bytes()
                    .all(|b| b.is_ascii_digit())
                {
                    // Go names subject-less credentials after the current time.
                    let stem = file_name.trim_start_matches("xai-").trim_end_matches(".json");
                    assert!(stem.bytes().all(|b| b.is_ascii_digit()), "{name}: {file_name}");
                } else {
                    assert_eq!(file_name, want_name, "{name}: file name");
                }
                let saved = std::fs::read_to_string(&path).unwrap();
                assert_eq!(normalize_times(&saved), case["file"].as_str().unwrap(), "{name}: file");
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
                }
            }
            Err(error) => {
                let got = String::from_utf8_lossy(&error.body).into_owned();
                let want = case["error"]
                    .as_str()
                    .unwrap_or_else(|| panic!("{name}: Rust failed with {got}"));
                if SYNTAX_MESSAGE.contains(&name) {
                    let prefix = want.split("parse response: ").next().unwrap();
                    assert!(got.starts_with(prefix), "{name}: {got} vs {want}");
                } else {
                    assert_eq!(got, without_quoted_number(want), "{name}: error");
                }
            }
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[tokio::test]
async fn refresh_matches_go_executor() {
    let f = fixture();
    for case in f["refresh"].as_array().unwrap() {
        let name = case["name"].as_str().unwrap();
        let mock = Mock::start(&case["script"]).await;
        let mut metadata: Map<String, Value> = case["metadata"].as_object().unwrap().clone();
        for value in metadata.values_mut() {
            if let Value::String(s) = value {
                *s = s.replace("UPSTREAM", &mock.addr);
            }
        }
        let credential = Credential::from_file(
            Path::new("/fixture"),
            Path::new("/fixture/xai-test.json"),
            metadata.clone(),
        )
        .unwrap();
        let result = refresh(&mock.auth(), &credential).await;
        let want_requests: Vec<String> = case["requests"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| r.as_str().unwrap().to_owned())
            .collect();
        assert_eq!(mock.requests(), want_requests, "{name}: requests");
        match result {
            Ok(patch) => {
                assert!(case["error"].is_null(), "{name}: Go failed with {}", case["error"]);
                patch.apply(&mut metadata);
                let got: Map<String, Value> = metadata
                    .into_iter()
                    .map(|(k, v)| match v {
                        Value::String(s) => (k, Value::String(normalize_times(&s.replace(&mock.addr, "UPSTREAM")))),
                        other => (k, other),
                    })
                    .collect();
                assert_eq!(Value::Object(got), case["metadata_out"], "{name}: metadata");
            }
            Err(error) => {
                let got = String::from_utf8_lossy(&error.body).into_owned();
                let want = case["error"]
                    .as_str()
                    .unwrap_or_else(|| panic!("{name}: Rust failed with {got}"));
                // Go appends the token endpoint's body to status errors; Rust withholds it.
                let want = match want.split_once(": {") {
                    Some((head, _)) if head.contains("failed with status") => head,
                    _ => want,
                };
                assert_eq!(got, without_quoted_number(want), "{name}: error");
            }
        }
    }
}

#[test]
fn number_mismatch_never_quotes_the_value() {
    let fields = [("expires_in", Field::Int)];
    for body in [
        &br#"{"expires_in":1.5}"#[..],
        br#"{"expires_in":99999999999999999999}"#,
        br#"{"expires_in":-1e3}"#,
    ] {
        let Err(error) = go_unmarshal(body, &fields, "T", "Wire") else {
            panic!("{body:?} decoded");
        };
        assert_eq!(
            error,
            "json: cannot unmarshal number into Go struct field Wire.expires_in of type int"
        );
    }
    assert_eq!(
        without_quoted_number("x: json: cannot unmarshal number 1.5 into Go struct field .e of type int"),
        "x: json: cannot unmarshal number into Go struct field .e of type int"
    );
}

#[test]
fn poll_interval_follows_go() {
    // PollForToken: the device interval, at least 5s; the test knob replaces a missing one.
    assert_eq!(initial_interval(0, None), Duration::from_secs(5));
    assert_eq!(initial_interval(3, None), Duration::from_secs(5));
    assert_eq!(initial_interval(9, None), Duration::from_secs(9));
    assert_eq!(
        initial_interval(-1, Some(Duration::from_millis(7))),
        Duration::from_millis(7)
    );
    assert_eq!(
        initial_interval(2, Some(Duration::from_millis(7))),
        Duration::from_secs(2)
    );
}

#[tokio::test]
async fn refresh_is_single_flighted_per_token() {
    let script = serde_json::json!([
        {"path": "/oauth2/token", "status": 200, "body": "{\"access_token\":\"a\"}"},
        {"path": "/oauth2/token", "status": 200, "body": "{\"access_token\":\"b\"}"}
    ]);
    let mock = Mock::start(&script).await;
    let auth = mock.auth();
    let endpoint = format!("http://{}/oauth2/token", mock.addr);
    let (a, b) = tokio::join!(
        auth.refresh("rt-shared", &endpoint),
        auth.refresh(" rt-shared ", &endpoint)
    );
    assert_eq!(a.unwrap().access_token, b.unwrap().access_token);
    assert_eq!(mock.requests().len(), 1);
}

#[tokio::test]
async fn slow_down_adds_the_poll_step() {
    // exchangeDeviceCode: slow_down adds minPollInterval when set, else 5s.
    let script = serde_json::json!([
        {"path": "/t", "status": 400, "body": "{\"error\":\"slow_down\"}"},
        {"path": "/t", "status": 400, "body": "{\"error\":\"slow_down\"}"}
    ]);
    let mock = Mock::start(&script).await;
    let endpoint = format!("http://{}/t", mock.addr);
    let mut interval = Duration::from_millis(10);
    let knob = mock.auth();
    assert_eq!(knob.exchange(&endpoint, "d", &mut interval).await.unwrap(), None);
    assert_eq!(interval, Duration::from_millis(11));
    let plain = XaiAuth::new(crate::proxy::default_client());
    assert_eq!(plain.exchange(&endpoint, "d", &mut interval).await.unwrap(), None);
    assert_eq!(interval, Duration::from_millis(5011));
}

#[test]
fn metadata_strings_follow_go_fmt_sprint() {
    for case in fixture()["sprint"].as_array().unwrap() {
        let (raw, want) = (case[0].as_str().unwrap(), case[1].as_str().unwrap());
        let mut metadata = Map::new();
        metadata.insert("type".into(), Value::from("xai"));
        metadata.insert("k".into(), serde_json::from_str(raw).unwrap());
        let credential = Credential::from_file(Path::new("/f"), Path::new("/f/x.json"), metadata).unwrap();
        assert_eq!(metadata_string(&credential, "k"), want, "fmt.Sprint({raw})");
    }
}

#[test]
fn token_expiry_wraps_like_go_durations() {
    let now = DateTime::from_timestamp(1_000_000_000, 0).unwrap();
    for case in fixture()["expiry"].as_array().unwrap() {
        let mut decoded = Decoded::default();
        decoded.strs.insert("access_token", "a".into());
        decoded.ints.insert("expires_in", case[0].as_i64().unwrap());
        assert_eq!(
            token_data(&decoded, now).expire,
            case[1].as_str().unwrap(),
            "expires_in {}",
            case[0]
        );
    }
}
