use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Context;
use clap::Parser;
use cpa_core::config::Config;
use cpa_exec::Executors;
use cpa_exec::claude::{ClaudeExecutor, DEFAULT_BASE_URL};
use cpa_server::{Runtime, router};
use tokio::net::TcpListener;

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
            let single_dash_long =
                i > 0 && a.len() > 2 && a.starts_with('-') && !a.starts_with("--");
            if single_dash_long { format!("-{a}") } else { a }
        })
        .collect()
}

/// An empty host binds every interface, as Go's `net.Listen("tcp", ":port")` does.
/// `[::]` is dual-stack on Linux by default; hosts without IPv6 fall back to IPv4.
async fn bind(host: &str, port: u16) -> std::io::Result<TcpListener> {
    if host.is_empty() {
        return match TcpListener::bind(("::", port)).await {
            Ok(listener) => Ok(listener),
            Err(_) => TcpListener::bind(("0.0.0.0", port)).await,
        };
    }
    TcpListener::bind((host, port)).await
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();
    let args = Args::parse_from(go_style_args());
    let config =
        Config::load(&args.config).with_context(|| format!("reading {}", args.config.display()))?;
    if config.api_keys.is_empty() {
        tracing::warn!(
            "access.api-keys is empty: the proxy API is open to anyone who can reach it"
        );
    }
    let credentials = cpa_core::credential::load_dir(&config.auth_dir)
        .with_context(|| format!("reading auth dir {}", config.auth_dir.display()))?;
    tracing::info!(credentials = credentials.len(), "credentials loaded");
    let listener = bind(&config.host, config.port)
        .await
        .with_context(|| format!("binding {}:{}", config.host, config.port))?;
    tracing::info!(addr = %listener.local_addr()?, "listening");
    let executors = Executors {
        claude: ClaudeExecutor::new(DEFAULT_BASE_URL)?,
    };
    let rt = Arc::new(Runtime::new(config, credentials, executors));
    axum::serve(listener, router(rt)).await?;
    Ok(())
}
