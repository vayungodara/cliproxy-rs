//! cliproxy's main() with one difference: Options.login_base points every provider's
//! login endpoints (Claude, Codex, Kimi, Meta) at a local fake. Test tool, never shipped.
//! Build: cp ../../../Cargo.lock . && CARGO_TARGET_DIR=../../../target cargo build
//! Run:   login-harness <config.yaml> http://127.0.0.1:9102   (see scripts/fake-logins.mjs)
use std::net::SocketAddr;
use std::sync::Arc;

use cpa_core::config::Config;
use cpa_exec::Executors;
use cpa_exec::claude::{ClaudeExecutor, DEFAULT_BASE_URL};
use cpa_server::{Runtime, router};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let mut args = std::env::args().skip(1);
    let path = std::path::PathBuf::from(args.next().expect("config path"));
    let login_base = args.next().expect("fake login base URL");
    let config = Config::load(&path)?;
    let credentials = cpa_core::config::credentials::load(&config);
    let listener = tokio::net::TcpListener::bind((config.host.as_str(), config.port)).await?;
    let executors = Executors {
        claude: ClaudeExecutor::new(DEFAULT_BASE_URL)?,
        codex: cpa_exec::codex::CodexExecutor::new()?,
        devices: Default::default(),
        openai: Default::default(),
        google: Default::default(),
    };
    let rt = Arc::new(Runtime::new(config, credentials, executors));
    cpa_server::install_registry(&rt);
    let options = cpa_server::management::Options { login_base: Some(login_base), ..Default::default() };
    let management = cpa_server::management::Management::with_options(rt.clone(), path, options);
    let _watcher = cpa_server::watching::start(&management);
    let app = router(rt)
        .merge(cpa_server::management::router(management))
        .layer(axum::middleware::from_fn(cpa_server::management::cors));
    axum::serve(listener, app.into_make_service_with_connect_info::<SocketAddr>()).await?;
    Ok(())
}
