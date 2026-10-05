//! The Codex client catalog refresh (internal/registry/codex_client_models_updater.go):
//! fetched every three hours from the router-for-me mirrors, validated, and installed in
//! `cpa_common::codex_catalog` only when it changed. The first fetch happens when the
//! catalog is first needed (see `cpa_server::model_updater`), and later ones ask for it
//! only if it changed ([`crate::catalog_etag`]).

use std::sync::Once;
use std::time::Duration;

use crate::catalog_etag::Etags;
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

static ETAGS: Etags = Etags::new();

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
    let Some((data, source, headers)) = fetch(client, sources, &ETAGS).await else {
        tracing::warn!("Codex client model refresh: fetch failed from all URLs, keeping current data");
        return false;
    };
    let Some(data) = data else {
        tracing::info!(%source, "Codex client model refresh: no changes detected");
        return false;
    };
    match cpa_common::codex_catalog::load(&data) {
        Ok(true) => {
            ETAGS.remember(&source, &headers);
            tracing::info!(%source, "Codex client model refresh: catalog updated");
            true
        }
        Ok(false) => {
            ETAGS.remember(&source, &headers);
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
/// catalog of at most 8 MiB, or 304 to the `ETag` it sent last (`None` data: unchanged).
async fn fetch(
    client: &wreq::Client,
    sources: &[&str],
    etags: &Etags,
) -> Option<(Option<Vec<u8>>, String, http::HeaderMap)> {
    for source in sources {
        let mut headers = GoHeaders::new();
        let etag = etags.get(source);
        if let Some(tag) = &etag {
            headers.set("If-None-Match", tag.clone());
        }
        let route = |_: &url::Url| {
            Ok(crate::proxy::Route {
                client: client.clone(),
                order: None,
            })
        };
        let response: Upstream =
            match crate::proxy::send_request(&route, wreq::Method::GET, source, headers, None, Some(FETCH_TIMEOUT))
                .await
            {
                Ok(response) => response,
                Err(_) => {
                    tracing::debug!(%source, "Codex client models fetch failed");
                    continue;
                }
            };
        if response.status == 304 && etag.is_some() {
            return Some((None, (*source).to_owned(), response.headers));
        }
        if response.status != 200 {
            tracing::debug!(%source, status = response.status, "Codex client models fetch returned non-200");
            continue;
        }
        let headers = response.headers;
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
        return Some((Some(data.to_vec()), (*source).to_owned(), headers));
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
        let etags = Etags::new();
        let fetched = fetch(&client, &[&bad, &good], &etags).await.unwrap();
        assert_eq!(
            (fetched.0.as_deref(), fetched.1.as_str()),
            (Some(catalog.as_bytes()), good.as_str())
        );
        assert!(fetch(&client, &[&invalid], &etags).await.is_none());
        let (_, revision) = cpa_common::codex_catalog::snapshot();
        mock.script("/good", vec![reply(200, catalog)]);
        assert!(
            !refresh(&client, &[&good]).await,
            "the embedded catalog is already current"
        );
        assert_eq!(cpa_common::codex_catalog::snapshot().1, revision);
    }

    /// The `ETag` of an accepted catalog is sent back, a 304 is "unchanged", and a
    /// source that never sent one gets an unconditional request.
    #[tokio::test]
    async fn refresh_is_conditional_after_an_etag() {
        let mock = Mock::start().await;
        let catalog = cpa_common::codex_client::CLIENT_MODELS_JSON;
        let reply = |status: u16, etag: Option<&str>, body: &str| Reply {
            status,
            headers: etag.map(|t| ("etag".to_owned(), t.to_owned())).into_iter().collect(),
            body: body.into(),
        };
        let url = format!("{}/c.json", mock.url);
        let etags = Etags::new();
        mock.script("/c.json", vec![reply(200, Some("\"v1\""), catalog)]);
        let (data, _, headers) = fetch(&wreq::Client::new(), &[&url], &etags).await.unwrap();
        assert!(data.is_some());
        etags.remember(&url, &headers);
        mock.script("/c.json", vec![reply(304, None, "")]);
        let (data, source, _) = fetch(&wreq::Client::new(), &[&url], &etags).await.unwrap();
        assert_eq!((data, source.as_str()), (None, url.as_str()));
        let sent: Vec<String> = mock
            .take()
            .iter()
            .map(|r| r.header("if-none-match").to_owned())
            .collect();
        assert_eq!(sent, ["", "\"v1\""]);
        // Without a stored ETag a 304 is not a catalog: the source is skipped.
        mock.script("/c.json", vec![reply(304, None, "")]);
        assert!(fetch(&wreq::Client::new(), &[&url], &Etags::new()).await.is_none());
    }
}
