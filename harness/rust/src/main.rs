//! Only test wiring: the production router/runtime/executor remain unchanged.
use std::path::Path;
use std::sync::Arc;

use cpa_core::config::Config;
use cpa_exec::Executors;
use cpa_exec::claude::{ClaudeExecutor, DEFAULT_BASE_URL};
use cpa_server::{Runtime, router};
use wreq::tls::trust::CertStore;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    anyhow::ensure!(args.len() == 3, "usage: driver CONFIG TEST_CA");
    let config = Config::load(Path::new(&args[1]))?;
    let credentials = cpa_core::config::credentials::load(&config);
    let trust = CertStore::from_pem_stack(std::fs::read(&args[2])?)?;
    // Dial routing changes neither the logical origin nor TLS/HTTP profile settings.
    let client = wreq::Client::builder()
        .no_proxy()
        .resolve("api.anthropic.com", "127.0.0.3:443".parse()?)
        .resolve("platform.claude.com", "127.0.0.3:443".parse()?)
        .tls_cert_store(trust)
        .build()?;
    let executors = Executors {
        claude: ClaudeExecutor::with_client(client, DEFAULT_BASE_URL),
    };
    let listener = tokio::net::TcpListener::bind((config.host.as_str(), config.port)).await?;
    let rt = Arc::new(Runtime::new(config, credentials, executors));
    cpa_server::install_registry(&rt);
    axum::serve(listener, router(rt)).await?;
    Ok(())
}
