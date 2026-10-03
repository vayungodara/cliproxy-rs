//! Codex Alpha Search through the real router, against a loopback upstream. Fake tokens only.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::{Arc, Mutex};

use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use cpa_core::config::Config;
use cpa_core::credential::Credential;
use cpa_exec::Executors;
use cpa_exec::codex::CodexExecutor;
use cpa_exec::codex_oauth::CodexOAuth;
use cpa_server::router;

type Seen = Arc<Mutex<Vec<(String, HeaderMap, Bytes)>>>;

async fn serve(app: axum::Router) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    url
}

fn credential(id: &str, meta: serde_json::Value, attrs: &[(&str, &str)]) -> Credential {
    let mut c = Credential::from_file(
        Path::new("/fake"),
        &Path::new("/fake").join(id),
        meta.as_object().unwrap().clone(),
    )
    .unwrap();
    c.attributes = attrs
        .iter()
        .map(|(k, v)| ((*k).into(), (*v).into()))
        .collect::<BTreeMap<_, _>>();
    c
}

#[tokio::test]
async fn alpha_search_uses_policy_eligible_credential_and_passes_upstream_through() {
    let seen: Seen = Arc::default();
    let upstream = axum::Router::new()
        .fallback(
            |State(seen): State<Seen>, uri: axum::http::Uri, headers: HeaderMap, body: Bytes| async move {
                seen.lock().unwrap().push((uri.path().to_owned(), headers, body));
                (
                    StatusCode::IM_A_TEAPOT,
                    [("content-type", "text/plain")],
                    "raw upstream body",
                )
                    .into_response()
            },
        )
        .with_state(seen.clone());
    let upstream_url = serve(upstream).await;
    let executor = CodexExecutor::with_client(wreq::Client::new(), CodexOAuth::new(wreq::Client::new()))
        .with_alpha_base_url(format!("{upstream_url}/backend-api/codex"));
    let credentials = vec![
        // An API key without alpha-search would be picked first by round-robin order.
        credential(
            "a-key.json",
            serde_json::json!({"type":"codex"}),
            &[
                ("api_key", "sk-FAKE"),
                ("base_url", &upstream_url),
                ("auth_kind", "apikey"),
            ],
        ),
        credential(
            "b-oauth.json",
            serde_json::json!({"type":"codex","access_token":"at-FAKE","account_id":"acct-FAKE"}),
            &[("auth_kind", "oauth")],
        ),
    ];
    let rt = Arc::new(cpa_server::testing::runtime(
        Config::parse("{}").unwrap(),
        credentials,
        Executors {
            claude: cpa_exec::claude::ClaudeExecutor::new("http://127.0.0.1:1").unwrap(),
            codex: executor,
            devices: Default::default(),
            openai: Default::default(),
            google: Default::default(),
        },
    ));
    let proxy = serve(router(rt)).await;
    // Go selects with the route model: no credential registers this one (auth_not_found).
    let unknown = wreq::Client::new()
        .post(format!("{proxy}/backend-api/codex/alpha/search"))
        // Duplicate `id` must not hide the model from selection (Go keeps both fields).
        .body(r#"{"id":"s-1","id":"s-1","model":"gpt-5.4","query":"q"}"#)
        .send()
        .await
        .unwrap();
    assert_eq!(unknown.status().as_u16(), 503);
    assert_eq!(
        unknown.text().await.unwrap(),
        r#"{"error":"auth_not_found: no auth available"}"#
    );
    assert!(seen.lock().unwrap().is_empty());
    let response = wreq::Client::new()
        .post(format!("{proxy}/backend-api/codex/alpha/search"))
        .header("user-agent", "codex_cli_rs/0.150.0")
        .header("version", "0.150.0")
        .header("session_id", "s-1")
        .header("authorization", "Bearer client-key-not-forwarded")
        .body(r#"{"id":"s-1","model":"gpt-5.5","query":"rust <ws>","prompt_cache_key":"k","prompt_cache_retention":"24h"}"#)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status().as_u16(), 418);
    assert_eq!(response.headers()["content-type"], "text/plain");
    assert_eq!(response.text().await.unwrap(), "raw upstream body");
    let seen = seen.lock().unwrap();
    assert_eq!(seen.len(), 1);
    let (path, headers, body) = &seen[0];
    assert_eq!(path, "/backend-api/codex/alpha/search");
    assert_eq!(
        String::from_utf8_lossy(body),
        r#"{"id":"s-1","model":"gpt-5.5","query":"rust \u003cws\u003e"}"#,
        "prompt cache fields stripped with Go's map re-marshal"
    );
    assert_eq!(headers["authorization"], "Bearer at-FAKE", "client key never forwarded");
    assert_eq!(headers["chatgpt-account-id"], "acct-FAKE");
    assert_eq!(headers["originator"], "codex_cli_rs");
    assert_eq!(headers["user-agent"], "codex_cli_rs/0.150.0");
    assert_eq!(headers["version"], "0.150.0");
    assert_eq!(headers["session_id"], "s-1");
}
