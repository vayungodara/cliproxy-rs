//! Routed images on Codex credentials (`collectRoutedImages`, `streamRoutedImages` in
//! sdk/api/handlers/openai/openai_images_handlers.go): Go runs them with
//! `WithDisallowFreeAuth`, so a Codex credential on the free plan
//! (`isFreeCodexAuth`: `plan_type` attribute `free`) is never selected, whatever its
//! priority. Fake credentials against a loopback upstream only.

use std::path::Path;
use std::sync::{Arc, Mutex};

use axum::http::HeaderMap;
use cpa_core::config::Config;
use cpa_core::credential::Credential;
use cpa_exec::Executors;

async fn serve(app: axum::Router) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    url
}

fn codex(id: &str, token: &str, plan: &str, priority: &str, base_url: &str) -> Credential {
    let metadata = serde_json::json!({
        "type": "codex",
        "access_token": token,
        "account_id": "acct",
        "expired": "2099-01-01T00:00:00Z",
    });
    let mut credential = Credential::from_file(
        Path::new("/fake"),
        &Path::new("/fake").join(id),
        metadata.as_object().unwrap().clone(),
    )
    .unwrap();
    for (key, value) in [("base_url", base_url), ("plan_type", plan), ("priority", priority)] {
        credential.attributes.insert(key.into(), value.into());
    }
    credential
}

/// A private, empty `auth-dir`, removed when dropped: credential loading never falls back
/// to `~/.cli-proxy-api`.
struct AuthDir(std::path::PathBuf);

impl AuthDir {
    fn new() -> Self {
        let dir = std::env::temp_dir().join(format!("cpa-it-auth-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }
}

impl Drop for AuthDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[tokio::test]
async fn routed_images_never_select_a_free_codex_credential() {
    let seen: Arc<Mutex<Vec<String>>> = Arc::default();
    let record = seen.clone();
    let upstream = serve(axum::Router::new().fallback(move |headers: HeaderMap| {
        let record = record.clone();
        async move {
            let token = headers
                .get("authorization")
                .and_then(|v| v.to_str().ok())
                .unwrap_or_default()
                .to_owned();
            record.lock().unwrap().push(token);
            (
                [("content-type", "application/json")],
                r#"{"created":1,"data":[{"b64_json":"aGk="}]}"#,
            )
        }
    }))
    .await;
    let credentials = vec![
        // The free plan ranks first; only the exclusion keeps it out.
        codex("codex-free.json", "at-FREE", "free", "10", &upstream),
        codex("codex-plus.json", "at-PLUS", "plus", "1", &upstream),
    ];
    let auth_dir = AuthDir::new();
    let cfg = Config::parse(&format!(
        "auth-dir: {}\naccess:\n  api-keys: [client-key]\n",
        auth_dir.0.display()
    ))
    .unwrap();
    let executors = Executors {
        claude: cpa_exec::claude::ClaudeExecutor::new("http://127.0.0.1:1").unwrap(),
        codex: cpa_exec::codex::CodexExecutor::new().unwrap(),
        devices: Default::default(),
        openai: Default::default(),
        google: Default::default(),
    };
    let rt = Arc::new(cpa_server::testing::runtime(cfg, credentials, executors));
    let proxy = serve(cpa_server::router(rt)).await;
    let response = wreq::Client::new()
        .post(format!("{proxy}/v1/images/generations"))
        .header("authorization", "Bearer client-key")
        .header("content-type", "application/json")
        .body(r#"{"model":"gpt-image-2","prompt":"a lighthouse"}"#)
        .send()
        .await
        .unwrap();
    let status = response.status().as_u16();
    let body = response.text().await.unwrap();
    assert_eq!(status, 200, "{body}");
    assert_eq!(
        *seen.lock().unwrap(),
        ["Bearer at-PLUS"],
        "only the paid plan serves routed images"
    );
}
