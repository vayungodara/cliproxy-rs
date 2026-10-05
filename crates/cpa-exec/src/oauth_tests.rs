use super::*;
use axum::extract::{Request, State};
use axum::response::IntoResponse;
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

#[derive(Default)]
struct Mock {
    calls: Mutex<Vec<(String, http::HeaderMap, Vec<u8>)>>,
    profile_status: u16,
    token_status: u16,
    /// Statuses for the next token calls, before `token_status` applies.
    token_statuses: Mutex<std::collections::VecDeque<u16>>,
    omit_refresh: bool,
    started: tokio::sync::Notify,
    release: tokio::sync::Notify,
    hold: bool,
    token_calls: AtomicUsize,
}

async fn handler(State(mock): State<Arc<Mock>>, request: Request) -> axum::response::Response {
    let (parts, body) = request.into_parts();
    let body = axum::body::to_bytes(body, 8192).await.unwrap();
    mock.calls
        .lock()
        .unwrap()
        .push((parts.uri.path().into(), parts.headers, body.to_vec()));
    if parts.uri.path() == "/token" {
        mock.token_calls.fetch_add(1, Ordering::SeqCst);
        mock.started.notify_one();
        if mock.hold {
            mock.release.notified().await;
        }
        let status = mock
            .token_statuses
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or(mock.token_status);
        if status != 0 {
            return (
                http::StatusCode::from_u16(status).unwrap(),
                [("retry-after", "2")],
                "fake-refresh-echo",
            )
                .into_response();
        }
        let mut response = json!({"access_token":"sk-ant-oat-new-fake", "expires_in":36000,
            "account":{"uuid":"token-account", "email_address":"token@example.test"},
            "organization":{"uuid":"token-org", "name":"Token Organization"}});
        if !mock.omit_refresh {
            response["refresh_token"] = "fake-rotated".into();
        }
        axum::Json(response).into_response()
    } else if parts.uri.path() == "/profile" {
        if mock.profile_status != 0 {
            return (
                http::StatusCode::from_u16(mock.profile_status).unwrap(),
                "fake-access-echo",
            )
                .into_response();
        }
        axum::Json(
            json!({"account":{"uuid":"profile-account", "email":"profile@example.test"},
            "organization":{"uuid":"profile-org", "name":"Profile Organization"}}),
        )
        .into_response()
    } else {
        axum::Json(json!({})).into_response()
    }
}

async fn service(mock: Arc<Mock>) -> OAuth {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let router = axum::Router::new().fallback(handler).with_state(mock);
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    OAuth::with_endpoints(
        wreq::Client::builder()
            .http1_only()
            .redirect(wreq::redirect::Policy::none())
            .build()
            .unwrap(),
        &format!("{base}/token"),
        &format!("{base}/profile"),
        &format!("{base}/roles"),
    )
}

fn credential() -> Credential {
    Credential::from_file(Path::new("/fake"), Path::new("/fake/claude.json"), json!({
        "type":"claude", "access_token":"sk-ant-oat-old-fake", "refresh_token":"fake-refresh",
        "email":"old@example.test", "account_uuid":"old-account", "organization_uuid":"old-org",
        "expired":"2000-01-01T00:00:00Z", "claude_device_ids":["invalid", format!(" {} ", "A".repeat(64)), "b".repeat(64)],
        "unknown":{"preserve":true}, "id_token":"fake-id"
    }).as_object().unwrap().clone()).unwrap()
}

#[test]
fn refresh_lead_and_legacy_refresh_spelling() {
    let now = DateTime::parse_from_rfc3339("2026-10-02T12:00:00Z")
        .unwrap()
        .with_timezone(&Utc);
    let mut credential = credential();
    credential
        .metadata
        .insert("expired".into(), "2026-10-02T16:00:01Z".into());
    assert!(!refresh_due(&credential, now));
    credential
        .metadata
        .insert("expired".into(), "2026-10-02T16:00:00Z".into());
    assert!(refresh_due(&credential, now));
    credential.metadata.insert("refresh_token".into(), "".into());
    assert!(!refresh_due(&credential, now));
    credential.metadata.insert("refreshToken".into(), "fake-legacy".into());
    assert!(refresh_due(&credential, now));
}

#[tokio::test]
async fn refresh_rotation_preserves_identity_on_optional_profile_failure() {
    let mock = Arc::new(Mock {
        profile_status: 503,
        ..Default::default()
    });
    let oauth = service(mock.clone()).await;
    let mut credential = credential();
    // Identity preparation never refreshes, even inside the refresh lead.
    assert!(refresh_due(&credential, Utc::now()));
    let prepared = oauth.prepare(&credential, &crate::proxy::Proxy::Inherit).await.unwrap();
    assert_eq!(prepared.set.keys().collect::<Vec<_>>(), ["claude_device_ids"]);
    prepared.apply(&mut credential.metadata);
    let patch = oauth
        .refresh_credential(&credential, &crate::proxy::Proxy::Inherit)
        .await
        .unwrap();
    patch.apply(&mut credential.metadata);
    assert!(!refresh_due(&credential, Utc::now()));
    assert_eq!(credential.str("access_token"), Some("sk-ant-oat-new-fake"));
    assert_eq!(credential.str("refresh_token"), Some("fake-rotated"));
    assert_eq!(credential.str("account_uuid"), Some("old-account"));
    assert_eq!(credential.str("email"), Some("old@example.test"));
    assert_eq!(credential.str("id_token"), Some("fake-id"));
    assert_eq!(credential.metadata["unknown"], json!({"preserve":true}));
    assert_eq!(credential.metadata["claude_device_ids"], json!(["a".repeat(64)]));
    assert!(!needs_prepare(&credential, chrono::Utc::now()));
    let calls = mock.calls.lock().unwrap();
    assert_eq!(calls.len(), 2);
    assert_eq!(calls[0].2, format!(r#"{{"client_id":"{CLIENT_ID}","grant_type":"refresh_token","refresh_token":"fake-refresh","scope":"{SCOPE}"}}"#).as_bytes());
    assert_eq!(calls[1].1["authorization"], "Bearer sk-ant-oat-new-fake");
}

#[tokio::test]
async fn missing_rotated_refresh_and_profile_fields_keep_saved_values() {
    let mock = Arc::new(Mock {
        omit_refresh: true,
        ..Default::default()
    });
    let oauth = service(mock).await;
    let patch = oauth.refresh("fake-refresh").await.unwrap();
    assert_eq!(patch.set["refresh_token"], "fake-refresh");
    assert_eq!(patch.set["account_uuid"], "profile-account");
    assert_eq!(patch.set["email"], "profile@example.test");
}

#[tokio::test]
async fn token_key_singleflight_survives_canceled_waiter() {
    let mock = Arc::new(Mock {
        hold: true,
        ..Default::default()
    });
    let oauth = service(mock.clone()).await;
    let first = oauth.clone();
    let waiter = tokio::spawn(async move { first.refresh("fake-refresh").await });
    mock.started.notified().await;
    waiter.abort();
    assert!(waiter.await.unwrap_err().is_cancelled());
    let second = oauth.clone();
    let waiter = tokio::spawn(async move { second.refresh("fake-refresh").await });
    mock.release.notify_one();
    let patch = waiter.await.unwrap().unwrap();
    assert_eq!(patch.set["refresh_token"], "fake-rotated");
    assert_eq!(mock.token_calls.load(Ordering::SeqCst), 1);
    // A finished exchange is not reused (Go singleflight): a forced refresh after a 401
    // exchanges again.
    mock.release.notify_one();
    oauth.refresh("fake-refresh").await.unwrap();
    assert_eq!(mock.token_calls.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn a_late_joiner_keeps_its_own_retry_budget() {
    // Go shares one exchange per token, not a caller's retry loop: B joins A's third
    // (failing) attempt and still retries on its own budget.
    let mock = Arc::new(Mock {
        hold: true,
        token_statuses: Mutex::new([503, 503, 503].into()),
        ..Default::default()
    });
    let oauth = service(mock.clone()).await;
    let refresh = |oauth: &OAuth| {
        let oauth = oauth.clone();
        tokio::spawn(async move { oauth.refresh("fake-refresh-late").await })
    };
    let a = refresh(&oauth);
    for _ in 0..2 {
        mock.started.notified().await;
        mock.release.notify_one();
    }
    mock.started.notified().await;
    let b = refresh(&oauth);
    tokio::time::sleep(Duration::from_millis(100)).await;
    mock.release.notify_one();
    assert_eq!(a.await.unwrap().unwrap_err().status, 503);
    tokio::time::timeout(Duration::from_secs(5), mock.started.notified())
        .await
        .expect("the late joiner retries on its own budget");
    mock.release.notify_one();
    let patch = b.await.unwrap().unwrap();
    assert_eq!(patch.set["refresh_token"], "fake-rotated");
    assert_eq!(mock.token_calls.load(Ordering::SeqCst), 4);
}

#[tokio::test]
async fn a_canceled_caller_stops_retrying() {
    let mock = Arc::new(Mock {
        token_statuses: Mutex::new([503].into()),
        ..Default::default()
    });
    let oauth = service(mock.clone()).await;
    let caller = {
        let oauth = oauth.clone();
        tokio::spawn(async move { oauth.refresh("fake-refresh-cancel").await })
    };
    mock.started.notified().await;
    // The first exchange has failed; the caller is in its one-second backoff.
    tokio::time::sleep(Duration::from_millis(300)).await;
    caller.abort();
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert_eq!(mock.token_calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn refresh_429_backoff_and_error_redaction() {
    let mock = Arc::new(Mock {
        token_status: 429,
        ..Default::default()
    });
    let oauth = service(mock.clone()).await;
    let error = oauth.refresh("fake-refresh-429").await.unwrap_err();
    assert_eq!(error.status, 429);
    assert_eq!(error.retry_after, Some(Duration::from_secs(5)));
    assert!(!error.to_string().contains("fake-refresh"));
    assert!(error.headers.is_empty());
    // Blocked until Retry-After: no second exchange.
    let error = oauth.refresh("fake-refresh-429").await.unwrap_err();
    assert_eq!(error.status, 429);
    assert!(
        error
            .retry_after
            .is_some_and(|d| d <= Duration::from_secs(5) && !d.is_zero())
    );
    assert_eq!(mock.token_calls.load(Ordering::SeqCst), 1);
    let mut headers = http::HeaderMap::new();
    headers.insert("retry-after-ms", "9000".parse().unwrap());
    assert_eq!(refresh_backoff(&headers), Duration::from_secs(9));
    headers.insert("retry-after", "9999".parse().unwrap());
    assert_eq!(refresh_backoff(&headers), Duration::from_secs(300));
}

#[tokio::test]
async fn code_exchange_login_layout_and_atomic_permissions() {
    let mock: Arc<Mock> = Arc::default();
    let oauth = service(mock.clone()).await;
    let patch = oauth
        .exchange("fake-code#state-fragment", "state-argument", "fake-verifier")
        .await
        .unwrap();
    let calls = mock.calls.lock().unwrap();
    assert_eq!(
        calls.iter().map(|c| c.0.as_str()).collect::<Vec<_>>(),
        ["/token", "/profile", "/roles"]
    );
    assert_eq!(calls[0].2, format!(r#"{{"grant_type":"authorization_code","code":"fake-code","redirect_uri":"{REDIRECT_URI}","client_id":"{CLIENT_ID}","code_verifier":"fake-verifier","state":"state-fragment"}}"#).as_bytes());
    drop(calls);
    for field in [
        "id_token",
        "access_token",
        "refresh_token",
        "last_refresh",
        "email",
        "type",
        "expired",
    ] {
        assert!(patch.set[field].is_string(), "{field} must always serialize");
    }
    assert!(canonical_pool(patch.set.get("claude_device_ids")));
}

#[test]
fn pkce_and_authorize_url() {
    let (verifier, challenge) = pkce().unwrap();
    assert_eq!(verifier.len(), 128);
    assert_eq!(challenge.len(), 43);
    let url = Url::parse(&authorize_url("good-state", &challenge)).unwrap();
    assert!(url.query_pairs().any(|(key, value)| key == "scope" && value == SCOPE));
}

#[tokio::test]
async fn raw_oauth_token_and_inspection_header_order_and_case() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let capture = tokio::spawn(async move {
        let mut captures = Vec::new();
        for index in 0..3 {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut bytes = Vec::new();
            while !bytes.ends_with(b"\r\n\r\n") {
                let mut byte = [0];
                socket.read_exact(&mut byte).await.unwrap();
                bytes.push(byte[0]);
            }
            let headers = String::from_utf8(bytes).unwrap();
            let length = headers
                .lines()
                .find_map(|line| line.strip_prefix("Content-Length: "))
                .map(|s| s.parse::<usize>().unwrap())
                .unwrap_or(0);
            let mut body = vec![0; length];
            socket.read_exact(&mut body).await.unwrap();
            captures.push((headers, body));
            let body = if index == 0 {
                r#"{"access_token":"fake-access","refresh_token":"fake-refresh","expires_in":36000}"#
            } else {
                "{}"
            };
            socket.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).as_bytes()).await.unwrap();
        }
        captures
    });
    let oauth = OAuth::with_endpoints(
        wreq::Client::builder().http1_only().build().unwrap(),
        &format!("{base}/token"),
        &format!("{base}/profile"),
        &format!("{base}/roles"),
    );
    oauth
        .exchange("fake-code", "fake-state", "fake-verifier")
        .await
        .unwrap();
    let captures = capture.await.unwrap();
    let names = |raw: &str| {
        raw.lines()
            .skip(1)
            .filter_map(|line| line.split_once(':').map(|(name, _)| name.to_owned()))
            .collect::<Vec<_>>()
    };
    assert_eq!(
        names(&captures[0].0),
        [
            "Accept",
            "Content-Type",
            "User-Agent",
            "Content-Length",
            "Accept-Encoding",
            "Host",
            "Connection"
        ]
    );
    for (headers, body) in &captures[1..] {
        assert_eq!(
            names(headers),
            [
                "Accept",
                "Content-Type",
                "Authorization",
                "Cache-Control",
                "User-Agent",
                "Accept-Encoding",
                "Host",
                "Connection"
            ]
        );
        assert!(headers.contains("Authorization: Bearer fake-access\r\n"));
        assert!(body.is_empty());
    }
}

#[tokio::test]
async fn many_credentials_refresh_without_an_admission_limit() {
    let mock = Arc::new(Mock::default());
    let oauth = service(mock.clone()).await;
    for i in 0..65 {
        oauth.refresh(&format!("fake-refresh-many-{i}")).await.unwrap();
    }
    assert_eq!(mock.token_calls.load(Ordering::SeqCst), 65);
}

#[tokio::test]
async fn prepare_forces_a_refresh_outside_the_lead_window() {
    // The runtime's refresh-and-retry after a 401 (Go tryRefreshAfterUnauthorized) calls
    // prepare on an identified credential whose token has not expired yet.
    let mock = Arc::new(Mock::default());
    let oauth = service(mock.clone()).await;
    let mut credential = credential();
    credential
        .metadata
        .insert("expired".into(), "2099-01-01T00:00:00Z".into());
    credential
        .metadata
        .insert("refresh_token".into(), "fake-refresh-forced".into());
    credential
        .metadata
        .insert("claude_device_ids".into(), json!(["a".repeat(64)]));
    assert!(
        !needs_prepare(&credential, chrono::Utc::now()),
        "not due and identified"
    );
    let patch = oauth.prepare(&credential, &crate::proxy::Proxy::Inherit).await.unwrap();
    assert_eq!(patch.set["access_token"], "sk-ant-oat-new-fake");
    assert_eq!(mock.token_calls.load(Ordering::SeqCst), 1);
}

/// Generated device IDs are random: like the Go golden, any 64-hex run that is not
/// a fixture ID (one repeated character) reads as `<device>`.
fn normalize_devices(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = String::new();
    let mut i = 0;
    while i < bytes.len() {
        let run = bytes[i..]
            .iter()
            .take_while(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(b))
            .count();
        if run >= 64 {
            let device = &text[i..i + 64];
            if device.bytes().all(|b| b == bytes[i]) {
                out.push_str(device);
            } else {
                out.push_str("<device>");
            }
            i += 64;
        } else if run > 0 {
            out.push_str(&text[i..i + run]);
            i += run;
        } else {
            let c = text[i..].chars().next().unwrap();
            out.push(c);
            i += c.len_utf8();
        }
    }
    out
}

/// Go `EnsureClaudeCredentialDevicePoolRequired` in Home mode, recorded by the
/// reference's zz_rustgolden_test.go: the KV calls (key from Home's auth index or the
/// credential's own), NX/XX writes, the pool, and the errors.
#[tokio::test]
async fn home_device_pools_match_go() {
    let golden: Value = serde_json::from_str(include_str!("claude/testdata/go_claude_device_pool_home.json")).unwrap();
    for case in golden["cases"].as_array().unwrap() {
        let scenario = &case["scenario"];
        let name = scenario["name"].as_str().unwrap();
        let mut attributes = std::collections::BTreeMap::new();
        let index = scenario["index"].as_str().unwrap();
        if !index.is_empty() {
            attributes.insert(
                cpa_core::config::credentials::HOME_AUTH_INDEX.to_owned(),
                index.to_owned(),
            );
        }
        let credential = Credential {
            id: scenario["id"].as_str().unwrap().into(),
            provider: "claude".into(),
            source: cpa_core::credential::Source::Config {
                section: "home".into(),
                index: 0,
            },
            disabled: false,
            label: String::new(),
            attributes,
            metadata: scenario["metadata"].as_object().cloned().unwrap_or_default(),
            revision: 0,
        };
        let raw_pool = credential.metadata.get("claude_device_ids");
        if canonical_pool(raw_pool) {
            // Go returns the canonical pool before consulting Home.
            assert_eq!(case["calls"], json!([]), "{name}");
            assert_eq!(raw_pool.unwrap(), &case["pool"], "{name}");
            continue;
        }
        let values = Arc::new(Mutex::new(std::collections::HashMap::new()));
        if let Some(preset) = scenario["preset"].as_object() {
            for (k, v) in preset {
                values.lock().unwrap().insert(k.clone(), v.as_str().unwrap().to_owned());
            }
        }
        let home = cpa_home::fake::FakeHome::start(crate::claude::kv_test::kv_home(values.clone())).await;
        let client = home.client();
        let candidate = normalize_pool(
            raw_pool
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(Value::as_str),
        );
        let got = home_device_pool(&client, &credential, candidate).await;
        let calls: Vec<Value> = home
            .commands()
            .iter()
            .filter_map(|c| crate::claude::kv_test::as_go_call(c))
            .map(|mut call| {
                if call[0] == "set" {
                    call[2] = normalize_devices(call[2].as_str().unwrap()).into();
                }
                call
            })
            .collect();
        assert_eq!(&calls, case["calls"].as_array().unwrap(), "{name}: calls");
        match got {
            Ok(device) => assert_eq!(json!([normalize_devices(&device)]), case["pool"], "{name}: pool"),
            Err(error) => {
                let text = String::from_utf8_lossy(&error.body).into_owned();
                let want = case["error"].as_str().unwrap();
                // Go's JSON decoder words its errors differently from serde.
                match want.split_once("decode Home KV value: ") {
                    Some((prefix, _)) => assert!(
                        text.starts_with(&format!("{prefix}decode Home KV value: ")),
                        "{name}: {text}"
                    ),
                    None => assert_eq!(text, want, "{name}: error"),
                }
            }
        }
    }
}
