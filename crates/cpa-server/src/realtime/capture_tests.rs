//! Request-log capture of the live call and hangup (Go live.go:309 and
//! capabilities.go:155): the request log a real request-logging stack writes, with the
//! fields Go's call sites pass (provider `codex`, `Auth.AccountInfo`,
//! `headersForLogging`, `callResponseHeaders`).

use std::path::{Path, PathBuf};
use std::time::Duration;

use axum::extract::Request;
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::IntoResponse;

use super::*;

async fn serve(app: axum::Router) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    format!("http://{addr}")
}

/// The upstream: calls answer 201 with a call ID and response headers request logs keep
/// or drop; hangups answer 200 with a JSON body.
async fn upstream() -> String {
    serve(axum::Router::new().fallback(|request: Request| async move {
        if request.uri().path().ends_with("/hangup") {
            let mut response = (StatusCode::OK, r#"{"ok":true}"#).into_response();
            response
                .headers_mut()
                .insert(header::CONTENT_TYPE, HeaderValue::from_static("application/json"));
            return response;
        }
        let mut response = (StatusCode::CREATED, "v=0\r\no=upstream-answer\r\n").into_response();
        let h = response.headers_mut();
        h.insert(header::CONTENT_TYPE, HeaderValue::from_static("application/sdp"));
        h.insert(header::LOCATION, HeaderValue::from_static("/v1/live/call-123"));
        h.insert("x-request-id", HeaderValue::from_static("req-1"));
        h.append("openai-request-id", HeaderValue::from_static("oa-1"));
        h.append("openai-request-id", HeaderValue::from_static("oa-2"));
        h.insert("x-other", HeaderValue::from_static("not-logged"));
        response
    }))
    .await
}

fn scratch() -> PathBuf {
    let dir = std::env::temp_dir().join(format!("cpa-live-capture-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// A server with request logging on, writing into `dir`.
async fn start(dir: &Path, upstream_url: &str) -> String {
    let executor = cpa_exec::codex::CodexExecutor::with_client(
        wreq::Client::new(),
        cpa_exec::codex_oauth::CodexOAuth::new(wreq::Client::new()),
    )
    .with_live_endpoints(format!("{upstream_url}/calls"), format!("{upstream_url}/v1"));
    let meta = serde_json::json!({
        "type": "codex", "access_token": "oauth-token", "email": " voice@example.com ",
        "account_id": "acct-1",
    });
    let credential = cpa_core::credential::Credential::from_file(
        Path::new("/fake"),
        Path::new("/fake/codex-voice.json"),
        meta.as_object().unwrap().clone(),
    )
    .unwrap();
    let rt = Arc::new(crate::testing::runtime(
        // A private auth-dir: the test never reads the user's.
        cpa_core::config::Config::parse(&format!(
            "auth-dir: '{}'\nobservability: {{logs: {{request-log: true}}}}\n",
            dir.join("auths").display()
        ))
        .unwrap(),
        vec![credential],
        cpa_exec::Executors {
            claude: cpa_exec::claude::ClaudeExecutor::new("http://127.0.0.1:1").unwrap(),
            codex: executor,
            devices: Default::default(),
            openai: Default::default(),
            google: Default::default(),
        },
    ));
    let management = crate::management::Management::with_options(
        rt.clone(),
        dir.join("config.yaml"),
        crate::management::Options {
            log_dir: Some(dir.to_owned()),
            management_password: Some(String::new()),
            ..Default::default()
        },
    );
    let app = axum::Router::new()
        .merge(routes_with(&rt, Arc::new(Live::default())))
        .with_state(rt);
    serve(crate::request_logging::router(&management, app)).await
}

/// The log whose text contains `marker`, once completely written (it ends with the
/// downstream response and stops changing).
async fn log_with(dir: &Path, marker: &str) -> String {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    loop {
        for entry in std::fs::read_dir(dir).unwrap().flatten() {
            if entry.path().extension().is_some_and(|e| e == "log")
                && let Ok(text) = std::fs::read_to_string(entry.path())
                && text.contains(marker)
                && text.contains("=== RESPONSE ===")
            {
                tokio::time::sleep(Duration::from_millis(50)).await;
                if std::fs::read_to_string(entry.path()).is_ok_and(|again| again == text) {
                    return text;
                }
            }
        }
        assert!(tokio::time::Instant::now() < deadline, "no request log with {marker:?}");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// The text of one `=== name ===` section, up to the next section.
fn section<'a>(log: &'a str, name: &str) -> &'a str {
    let start = log
        .find(&format!("=== {name} ==="))
        .unwrap_or_else(|| panic!("no {name} in {log}"));
    let rest = &log[start..];
    let end = rest[4..].find("\n=== ").map_or(rest.len(), |i| i + 4);
    &rest[..end]
}

#[tokio::test]
async fn call_and_hangup_are_captured_like_go() {
    let dir = scratch();
    let upstream_url = upstream().await;
    let proxy = start(&dir, &upstream_url).await;
    let body = r#"{"sdp":"v=0","session":{"model":"gpt-live-1-codex"}}"#;
    let response = wreq::Client::new()
        .post(format!("{proxy}/v1/live"))
        .header("content-type", "application/json")
        .header("x-oai-attestation", "attestation-secret")
        .header("openai-alpha", "quicksilver=v2")
        .body(body)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status().as_u16(), 201);

    let log = log_with(&dir, "/calls").await;
    let request = section(&log, "API REQUEST 1");
    assert!(
        request.contains(&format!("Upstream URL: {upstream_url}/calls\n")),
        "{request}"
    );
    assert!(request.contains("HTTP Method: POST\n"), "{request}");
    // AccountInfo: OAuth, so the email is never printed.
    assert!(
        request.contains("Auth: provider=codex, auth_id=codex-voice.json, label=voice@example.com, type=oauth\n"),
        "{request}"
    );
    // headersForLogging keeps the header and hides its value; the bearer is masked.
    assert!(request.contains("X-Oai-Attestation: [REDACTED]\n"), "{request}");
    // The downstream HEADERS section logs the client's own headers, as in Go.
    assert!(
        !request.contains("attestation-secret") && !request.contains("oauth-token"),
        "{request}"
    );
    assert!(request.contains("Chatgpt-Account-Id: acct-1\n"), "{request}");
    assert!(request.contains("Openai-Alpha: quicksilver=v2\n"), "{request}");
    assert!(request.contains("Content-Type: application/json\n"), "{request}");
    // The body sent upstream: the model rewritten for Codex.
    let sent = &request[request.find("\nBody:\n").unwrap() + 7..];
    assert!(sent.starts_with(r#"{"sdp":"v=0","session":{"model":"#), "{sent}");
    let answer = section(&log, "API RESPONSE 1");
    assert!(answer.contains("Status: 201\n"), "{answer}");
    for line in [
        "Content-Type: application/sdp\n",
        "Location: /v1/live/call-123\n",
        "X-Request-Id: req-1\n",
        // Go's `Header.Add` canonicalizes the name.
        "Openai-Request-Id: oa-1\n",
        "Openai-Request-Id: oa-2\n",
    ] {
        assert!(answer.contains(line), "{line:?} in {answer}");
    }
    assert!(
        !answer.contains("X-Other") && !answer.contains("not-logged"),
        "{answer}"
    );
    assert!(answer.contains("Body:\nv=0\r\no=upstream-answer"), "{answer}");

    let hangup = wreq::Client::new()
        .post(format!("{proxy}/v1/realtime/calls/call-123/hangup"))
        .header("content-type", " application/json ")
        .body("{}")
        .send()
        .await
        .unwrap();
    assert_eq!(hangup.status().as_u16(), 200);
    let log = log_with(&dir, "/hangup").await;
    let request = section(&log, "API REQUEST 1");
    assert!(
        request.contains(&format!(
            "Upstream URL: {upstream_url}/v1/realtime/calls/call-123/hangup\n"
        )),
        "{request}"
    );
    assert!(request.contains("HTTP Method: POST\n"), "{request}");
    assert!(
        request.contains("Auth: provider=codex, auth_id=codex-voice.json"),
        "{request}"
    );
    assert!(request.contains("Content-Type: application/json\n"), "{request}");
    assert!(request.contains("\nBody:\n{}"), "{request}");
    let answer = section(&log, "API RESPONSE 1");
    assert!(answer.contains("Status: 200\n"), "{answer}");
    assert!(answer.contains("Body:\n{\"ok\":true}"), "{answer}");
    let _ = std::fs::remove_dir_all(dir);
}

/// `RecordAPIResponseError` when the upstream cannot be reached.
#[tokio::test]
async fn transport_errors_are_captured() {
    let dir = scratch();
    let proxy = start(&dir, "http://127.0.0.1:9").await;
    let response = wreq::Client::new()
        .post(format!("{proxy}/v1/live"))
        .header("content-type", "application/json")
        .body(r#"{"sdp":"v=0"}"#)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status().as_u16(), 502);
    let log = log_with(&dir, "127.0.0.1:9/calls").await;
    let answer = section(&log, "API RESPONSE 1");
    assert!(answer.contains("\nError: "), "{answer}");
    assert!(!answer.contains("Status:"), "{answer}");
    let _ = std::fs::remove_dir_all(dir);
}

/// SDP ICE credentials never reach the request log, in any section, while the client and
/// the upstream still get them byte for byte.
#[tokio::test]
async fn sdp_ice_credentials_stay_out_of_the_whole_log() {
    const ANSWER: &str = "v=0\r\na=ice-ufrag:answerufrag\r\na=ice-pwd:answer+pwd/secret\r\n";
    let dir = scratch();
    let received = Arc::new(std::sync::Mutex::new(Vec::<u8>::new()));
    let upstream_url = serve(axum::Router::new().fallback({
        let received = received.clone();
        move |body: axum::body::Bytes| async move {
            *received.lock().unwrap() = body.to_vec();
            let mut response = (StatusCode::CREATED, ANSWER).into_response();
            let h = response.headers_mut();
            h.insert(header::CONTENT_TYPE, HeaderValue::from_static("application/sdp"));
            h.insert(header::LOCATION, HeaderValue::from_static("/v1/live/call-123"));
            response
        }
    }))
    .await;
    let proxy = start(&dir, &upstream_url).await;
    let body = r#"{"sdp":"v=0\r\na=ice-ufrag:offerufrag\r\na=ice-pwd:offer+pwd/secret\r\n","session":{"model":"gpt-live-1-codex"}}"#;
    let response = wreq::Client::new()
        .post(format!("{proxy}/v1/live"))
        .header("content-type", "application/json")
        .body(body)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status().as_u16(), 201);
    assert_eq!(
        response.text().await.unwrap(),
        ANSWER,
        "the client gets the answer unchanged"
    );
    let sent = String::from_utf8(received.lock().unwrap().clone()).unwrap();
    assert!(
        sent.contains(r"a=ice-ufrag:offerufrag\r\na=ice-pwd:offer+pwd/secret\r\n"),
        "the upstream gets the offer's credentials: {sent}"
    );

    let log = log_with(&dir, "/calls").await;
    for secret in ["offerufrag", "offer+pwd", "answerufrag", "answer+pwd", "pwd/secret"] {
        assert!(!log.contains(secret), "{secret} in {log}");
    }
    for (name, line) in [
        ("REQUEST BODY", r"a=ice-ufrag:[REDACTED]\r\na=ice-pwd:[REDACTED]\r\n"),
        ("API REQUEST 1", r"a=ice-ufrag:[REDACTED]\r\na=ice-pwd:[REDACTED]\r\n"),
        ("API RESPONSE 1", "a=ice-ufrag:[REDACTED]\r\na=ice-pwd:[REDACTED]"),
        ("RESPONSE", "a=ice-ufrag:[REDACTED]\r\na=ice-pwd:[REDACTED]\r\n"),
    ] {
        let text = section(&log, name);
        assert!(text.contains(line), "{name}: {text}");
    }
    let _ = std::fs::remove_dir_all(dir);
}

/// Bodies past the in-memory limit go to the spool file; their ICE credentials are
/// redacted before they get there.
#[tokio::test]
async fn sdp_ice_credentials_stay_out_of_spooled_sections() {
    let padding = "a=x-pad:0123456789abcdef0123456789abcdef0123456789abcdef\r\n".repeat(2048);
    let answer = format!("v=0\r\n{padding}a=ice-ufrag:answerufrag\r\na=ice-pwd:answerpwd\r\n");
    assert!(answer.len() > 100 << 10, "past the in-memory limit");
    let dir = scratch();
    let upstream_url = serve(axum::Router::new().fallback({
        let answer = answer.clone();
        // Reads the whole offer first, as a real server would before answering.
        move |_offer: axum::body::Bytes| async move {
            let mut response = (StatusCode::CREATED, answer).into_response();
            let h = response.headers_mut();
            h.insert(header::CONTENT_TYPE, HeaderValue::from_static("application/sdp"));
            h.insert(header::LOCATION, HeaderValue::from_static("/v1/live/call-123"));
            response
        }
    }))
    .await;
    let proxy = start(&dir, &upstream_url).await;
    let offer = format!("v=0\r\n{padding}a=ice-ufrag:offerufrag\r\na=ice-pwd:offerpwd\r\n");
    let response = wreq::Client::new()
        .post(format!("{proxy}/v1/live"))
        .header("content-type", "application/sdp")
        .body(offer)
        .send()
        .await
        .unwrap();
    let status = response.status().as_u16();
    let text = response.text().await.unwrap();
    assert_eq!(status, 201, "{text}");
    assert_eq!(text, answer);
    let log = log_with(&dir, "/calls").await;
    for secret in ["offerufrag", "offerpwd", "answerufrag", "answerpwd"] {
        assert!(!log.contains(secret), "{secret} in the log");
    }
    for name in ["API REQUEST 1", "API RESPONSE 1"] {
        let text = section(&log, name);
        assert!(text.contains("a=x-pad:"), "{name} logged in full");
        assert!(text.contains("a=ice-pwd:[REDACTED]"), "{name} redacted");
    }
    let _ = std::fs::remove_dir_all(dir);
}

/// A live response the request log streams to its spool (an event-stream content type)
/// is redacted too when the log is written.
#[tokio::test]
async fn sdp_ice_credentials_stay_out_of_a_spooled_downstream_response() {
    const ANSWER: &str = "v=0\r\na=ice-ufrag:streamufrag\r\na=ice-pwd:streampwd\r\n";
    let dir = scratch();
    let upstream_url = serve(axum::Router::new().fallback(|_offer: axum::body::Bytes| async move {
        let mut response = (StatusCode::CREATED, ANSWER).into_response();
        let h = response.headers_mut();
        h.insert(header::CONTENT_TYPE, HeaderValue::from_static("text/event-stream"));
        h.insert(header::LOCATION, HeaderValue::from_static("/v1/live/call-123"));
        response
    }))
    .await;
    let proxy = start(&dir, &upstream_url).await;
    let response = wreq::Client::new()
        .post(format!("{proxy}/v1/live"))
        .header("content-type", "application/sdp")
        .body("v=0\r\n")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status().as_u16(), 201);
    assert_eq!(
        response.text().await.unwrap(),
        ANSWER,
        "the client gets the answer unchanged"
    );
    let log = log_with(&dir, "/calls").await;
    for secret in ["streamufrag", "streampwd"] {
        assert!(!log.contains(secret), "{secret} in {log}");
    }
    let downstream = section(&log, "RESPONSE");
    assert!(
        downstream.contains("a=ice-ufrag:[REDACTED]\r\na=ice-pwd:[REDACTED]"),
        "{downstream}"
    );
    let _ = std::fs::remove_dir_all(dir);
}
