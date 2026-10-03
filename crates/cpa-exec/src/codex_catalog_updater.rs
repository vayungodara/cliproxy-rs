//! The Codex client catalog refresh (internal/registry/codex_client_models_updater.go):
//! fetched at startup and every three hours from the router-for-me mirrors, validated,
//! and installed in `cpa_common::codex_catalog` only when it changed.

use std::sync::Once;
use std::time::Duration;

use crate::proxy::{GoHeaders, Upstream};

/// `codexClientModelsURLs`, tried in order.
pub const SOURCES: [&str; 2] = [
    "https://raw.githubusercontent.com/router-for-me/models/refs/heads/main/codex_client_models.json",
    "https://models.router-for.me/codex_client_models.json",
];
/// `maxCodexClientModelsSize`.
const MAX_SIZE: usize = 8 << 20;
/// `modelsFetchTimeout`.
const FETCH_TIMEOUT: Duration = Duration::from_secs(30);
/// `modelsRefreshInterval`.
const REFRESH_INTERVAL: Duration = Duration::from_secs(3 * 3600);

/// `StartCodexClientModelsUpdater`: one background refresh loop per process, starting
/// with an immediate fetch. Call from inside the Tokio runtime; later calls do nothing.
pub fn start_codex_client_models_updater() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        tokio::spawn(async {
            let client = crate::proxy::default_client();
            let mut ticker = tokio::time::interval(REFRESH_INTERVAL);
            loop {
                // The first tick completes at once: Go's startup refresh.
                ticker.tick().await;
                refresh(&client, &SOURCES).await;
            }
        });
    });
}

/// `tryRefreshCodexClientModels`: whether a fetched catalog replaced the current one.
pub async fn refresh(client: &wreq::Client, sources: &[&str]) -> bool {
    let Some((data, source)) = fetch(client, sources).await else {
        tracing::warn!("Codex client model refresh: fetch failed from all URLs, keeping current data");
        return false;
    };
    match cpa_common::codex_catalog::load(&data) {
        Ok(true) => {
            tracing::info!(%source, "Codex client model refresh: catalog updated");
            true
        }
        Ok(false) => {
            tracing::info!(%source, "Codex client model refresh: no changes detected");
            false
        }
        Err(error) => {
            tracing::warn!(%source, %error, "Codex client model refresh: fetched catalog rejected, keeping current data");
            false
        }
    }
}

/// `fetchCodexClientModelsFromRemote`: the first source answering 200 with a valid
/// catalog of at most 8 MiB.
async fn fetch(client: &wreq::Client, sources: &[&str]) -> Option<(Vec<u8>, String)> {
    for source in sources {
        let route = |_: &url::Url| {
            Ok(crate::proxy::Route {
                client: client.clone(),
                order: None,
            })
        };
        let response: Upstream = match crate::proxy::send_request(
            &route,
            wreq::Method::GET,
            source,
            GoHeaders::new(),
            None,
            Some(FETCH_TIMEOUT),
        )
        .await
        {
            Ok(response) => response,
            Err(_) => {
                tracing::debug!(%source, "Codex client models fetch failed");
                continue;
            }
        };
        if response.status != 200 {
            tracing::debug!(%source, status = response.status, "Codex client models fetch returned non-200");
            continue;
        }
        let Ok(data) = crate::proxy::read_all(response.body, MAX_SIZE + 1, false).await else {
            tracing::debug!(%source, "Codex client models fetch read error");
            continue;
        };
        if data.len() > MAX_SIZE {
            tracing::warn!(%source, "Codex client models fetch exceeded {MAX_SIZE} bytes");
            continue;
        }
        if let Err(error) = cpa_common::codex_catalog::validate(&data) {
            tracing::warn!(%source, %error, "Codex client models validate failed");
            continue;
        }
        return Some((data.to_vec(), (*source).to_owned()));
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codex_testkit::{Mock, Reply};

    /// Local mirrors only: a failing first source falls through to the second, an
    /// invalid catalog is never installed, and an identical one is not a change.
    #[tokio::test]
    async fn refresh_falls_through_sources_and_validates() {
        let mock = Mock::start().await;
        let catalog = cpa_common::codex_client::CLIENT_MODELS_JSON;
        let reply = |status: u16, body: &str| Reply {
            status,
            headers: vec![("content-type".into(), "application/json".into())],
            body: body.into(),
        };
        mock.script("/bad", vec![reply(500, "{}")]);
        mock.script("/good", vec![reply(200, catalog)]);
        mock.script("/invalid", vec![reply(200, r#"{"models":[]}"#)]);
        let client = wreq::Client::new();
        let (bad, good, invalid) = (
            format!("{}/bad", mock.url),
            format!("{}/good", mock.url),
            format!("{}/invalid", mock.url),
        );
        let fetched = fetch(&client, &[&bad, &good]).await.unwrap();
        assert_eq!(
            (fetched.0.as_slice(), fetched.1.as_str()),
            (catalog.as_bytes(), good.as_str())
        );
        assert!(fetch(&client, &[&invalid]).await.is_none());
        let (_, revision) = cpa_common::codex_catalog::snapshot();
        mock.script("/good", vec![reply(200, catalog)]);
        assert!(
            !refresh(&client, &[&good]).await,
            "the embedded catalog is already current"
        );
        assert_eq!(cpa_common::codex_catalog::snapshot().1, revision);
    }
}
