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
