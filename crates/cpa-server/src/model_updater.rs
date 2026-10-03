//! Remote model catalog refresh (Go internal/registry/model_updater.go
//! `StartModelsUpdater`, cmd/server/main.go `modelCatalogUpdaterPlan`): fetch
//! models.json at startup and every three hours, keep the current catalog when every
//! source fails, and swap in a valid changed one (`cpa_core::registry::refresh_catalog`).
//! The runtime's registry rebuilds from the new catalog on its next use, which is what
//! Go's refresh callback does by re-registering the affected credentials.

use std::time::Duration;

/// Go `modelsURLs`, tried in order.
pub const MODELS_URLS: [&str; 2] = [
    "https://raw.githubusercontent.com/router-for-me/models/refs/heads/main/models.json",
    "https://models.router-for.me/models.json",
];

/// Go `modelsFetchTimeout`.
const FETCH_TIMEOUT: Duration = Duration::from_secs(30);
/// Go `modelsRefreshInterval`.
const REFRESH_INTERVAL: Duration = Duration::from_secs(3 * 3600);

/// Which catalogs refresh (Go `modelCatalogUpdaterPlan`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Plan {
    /// models.json.
    pub models: bool,
    /// The Codex client model list.
    pub codex_client: bool,
    /// The Devin catalog.
    pub devin: bool,
}

/// `--local-model` disables every updater; Home mode disables only models.json because
/// Codex client metadata and Devin models stay edge-local.
pub fn plan(local_model: bool, home_enabled: bool) -> Plan {
    if local_model {
        return Plan {
            models: false,
            codex_client: false,
            devin: false,
        };
    }
    Plan {
        models: !home_enabled,
        codex_client: true,
        devin: true,
    }
}

/// Go `startModelCatalogUpdaters`. Safe to call more than once: one updater runs.
pub fn start(local_model: bool, home_enabled: bool) {
    let plan = plan(local_model, home_enabled);
    // ponytail: the Codex client catalog updater (codex_client_models_updater.go, owner:
    // Codex thread) is not on this base yet; start it here, before Devin, when
    // `plan.codex_client` is set.
    if plan.devin {
        cpa_exec::devin_models::start_devin_models_updater();
    }
    if plan.models {
        static STARTED: std::sync::OnceLock<()> = std::sync::OnceLock::new();
        STARTED.get_or_init(|| {
            let urls = MODELS_URLS.iter().map(|u| (*u).to_owned()).collect();
            tokio::spawn(run(urls, REFRESH_INTERVAL));
        });
    } else if home_enabled {
        tracing::info!(
            "Home mode: remote models.json updates disabled; Codex client model list follows Home model IDs"
        );
    }
}

/// Go `runModelsUpdater`: one refresh now, then one per `interval`.
pub async fn run(urls: Vec<String>, interval: Duration) {
    refresh(&urls, "startup model refresh").await;
    tracing::info!(
        "periodic model refresh started (interval={}h)",
        interval.as_secs() / 3600
    );
    let mut ticker = tokio::time::interval_at(tokio::time::Instant::now() + interval, interval);
    loop {
        ticker.tick().await;
        refresh(&urls, "periodic model refresh").await;
    }
}

/// Go `tryRefreshModels` with `fetchModelsFromRemote`: the first source that answers 200
/// with a valid catalog wins; a parse or validation failure moves on to the next.
pub async fn refresh(urls: &[String], label: &str) {
    let client = match wreq::Client::builder().timeout(FETCH_TIMEOUT).build() {
        Ok(client) => client,
        Err(e) => {
            tracing::warn!("{label}: client setup failed: {e}");
            return;
        }
    };
    for url in urls {
        let body = match client.get(url).send().await {
            Ok(resp) if resp.status().as_u16() == 200 => match resp.text().await {
                Ok(body) => body,
                Err(e) => {
                    tracing::debug!("models fetch read error from {url}: {e}");
                    continue;
                }
            },
            Ok(resp) => {
                tracing::debug!("models fetch returned {} from {url}", resp.status().as_u16());
                continue;
            }
            Err(e) => {
                tracing::debug!("models fetch failed from {url}: {e}");
                continue;
            }
        };
        match cpa_core::registry::refresh_catalog(&body) {
            Ok(changed) if changed.is_empty() => {
                tracing::info!("{label} completed from {url}, no changes detected");
                return;
            }
            Ok(changed) => {
                tracing::info!("{label} completed from {url}, changes detected for providers: {changed:?}");
                return;
            }
            Err(e) => tracing::warn!("models validate failed from {url}: {e}"),
        }
    }
    tracing::warn!("{label}: fetch failed from all URLs, keeping current data");
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Go `modelCatalogUpdaterPlan` (cmd/server/main.go).
    #[test]
    fn plan_matches_go() {
        let p = |models, codex_client, devin| Plan {
            models,
            codex_client,
            devin,
        };
        assert_eq!(plan(true, false), p(false, false, false));
        assert_eq!(plan(true, true), p(false, false, false));
        assert_eq!(plan(false, true), p(false, true, true));
        assert_eq!(plan(false, false), p(true, true, true));
    }
}
