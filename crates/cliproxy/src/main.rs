use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Context;
use clap::Parser;
use cpa_core::config::Config;
use cpa_server::{AppState, router};

/// Flags mirror CLIProxyAPI. Go's single-dash long flags (`-config`) are accepted.
#[derive(Parser)]
#[command(version, about)]
struct Args {
    /// Configure File Path
    #[arg(long, default_value = "config.yaml")]
    config: PathBuf,
}

/// Go's flag package treats `-name` and `--name` the same; clap needs `--name`.
fn go_style_args() -> Vec<String> {
    std::env::args()
        .enumerate()
        .map(|(i, a)| {
            let single_dash_long = i > 0 && a.len() > 2 && a.starts_with('-') && !a.starts_with("--");
            if single_dash_long { format!("-{a}") } else { a }
        })
        .collect()
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .init();
    let args = Args::parse_from(go_style_args());
    let config = Config::load(&args.config).with_context(|| format!("reading {}", args.config.display()))?;
    if config.api_keys.is_empty() {
        tracing::warn!("access.api-keys is empty: the proxy API is open to anyone who can reach it");
    }
    let addr = format!("{}:{}", config.host, config.port);
    let state = AppState::load(config, cpa_exec::claude::DEFAULT_BASE_URL)?;
    tracing::info!(claude = state.claude_credentials().len(), "credentials loaded");
    let listener = tokio::net::TcpListener::bind(&addr).await.with_context(|| format!("binding {addr}"))?;
    tracing::info!(%addr, "listening");
    axum::serve(listener, router(Arc::new(state))).await?;
    Ok(())
}
