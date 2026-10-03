//! Only test wiring. Mirrors crates/cliproxy/src/main.rs (config, credential synthesis,
//! auto-refresh, routes, management, CORS and the access log) except the listener and
//! two hooks: the ephemeral test CA replaces the trust store, and the logical
//! first-party hosts are dialed at 127.0.0.3. The executor still builds its production
//! native and OAuth clients (TLS profile, header order, session caches), including for
//! refresh, and its Go standard-transport clients for other origins.
use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;

use cpa_core::config::Config;
use cpa_exec::Executors;
use cpa_exec::claude::{ClaudeExecutor, DEFAULT_BASE_URL, Hooks};
use cpa_server::Runtime;
use wreq::tls::trust::CertStore;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    anyhow::ensure!(args.len() == 3, "usage: driver CONFIG TEST_CA");
    let config_path = Path::new(&args[1]);
    let config = Config::load(config_path)?;
    let credentials = cpa_core::config::credentials::load(&config);
    let upstream: SocketAddr = "127.0.0.3:443".parse()?;
    let hooks = Hooks {
        trust: Some(CertStore::from_pem_stack(std::fs::read(&args[2])?)?),
        resolve: vec![
            ("api.anthropic.com".into(), upstream),
            ("platform.claude.com".into(), upstream),
        ],
    };
    let executors = Executors {
        claude: ClaudeExecutor::with_hooks(hooks, DEFAULT_BASE_URL),
        codex: Default::default(),
        openai: Default::default(),
        google: Default::default(),
        devices: Default::default(),
    };
    let listener = tokio::net::TcpListener::bind((config.host.as_str(), config.port)).await?;
    let rt = Arc::new(Runtime::new(config, credentials, executors));
    cpa_server::install_registry(&rt);
    rt.start_auto_refresh();
    let management = cpa_server::management::Management::with_options(
        rt.clone(),
        config_path.to_owned(),
        cpa_server::management::Options::default(),
    );
    let _watcher = cpa_server::watching::start(&management);
    let app = cpa_server::app(rt.clone(), cpa_server::management::router(management.clone()))
        .layer(axum::middleware::from_fn(cpa_server::management::cors));
    let app = cpa_server::observability::router(&rt, app);
    // The production listener (crates/cliproxy uses the same call), plain HTTP.
    cpa_server::listener::serve_with_resp(listener, app, None, Some(management)).await?;
    Ok(())
}
