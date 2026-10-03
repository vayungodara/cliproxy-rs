use std::io;
use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr};
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Context;
use clap::Parser;
use cpa_core::config::Config;
use cpa_exec::Executors;
use cpa_exec::claude::{ClaudeExecutor, DEFAULT_BASE_URL};
use cpa_server::{Runtime, router};
use socket2::{Domain, Socket, Type};

/// Flags mirror CLIProxyAPI. Go's single-dash long flags (`-config`) are accepted.
#[derive(Parser)]
#[command(version, about)]
struct Args {
    /// Configure File Path
    #[arg(long, default_value = "config.yaml")]
    config: PathBuf,
    /// Log in to Claude using browser OAuth and PKCE.
    #[arg(long)]
    claude_login: bool,
    /// Login to Codex using OAuth
    #[arg(long)]
    codex_login: bool,
    /// Login to Codex using device code flow
    #[arg(long)]
    codex_device_login: bool,
    /// Don't open browser automatically for OAuth
    #[arg(long)]
    no_browser: bool,
    /// Override OAuth callback port (defaults to provider-specific port)
    #[arg(long, default_value_t = 0)]
    oauth_callback_port: u16,
    /// Login to Kimi (.com) using OAuth
    #[arg(long)]
    kimi_login: bool,
    /// Login to Kimi.ai using OAuth
    #[arg(long)]
    kimi_ai_login: bool,
    /// Login to xAI using OAuth
    #[arg(long)]
    xai_login: bool,
    /// Login to Meta using OAuth
    #[arg(long)]
    meta_login: bool,
    /// Management password accepted from loopback clients only.
    #[arg(long, hide = true, default_value = "")]
    password: String,
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

/// Binds like Go's `net.Listen("tcp", host:port)`. An empty host is one dual-stack socket
/// on `[::]` with `IPV6_V6ONLY` cleared, falling back to `0.0.0.0` only when the host has
/// no usable IPv6. Other errors, such as the port being in use, are returned.
fn bind(host: &str, port: u16) -> io::Result<std::net::TcpListener> {
    if !host.is_empty() {
        let listener = std::net::TcpListener::bind((host, port))?;
        listener.set_nonblocking(true)?;
        return Ok(listener);
    }
    match listen(Domain::IPV6, SocketAddr::from((Ipv6Addr::UNSPECIFIED, port)), true) {
        Err(e) if matches!(e.raw_os_error(), Some(libc::EAFNOSUPPORT | libc::EADDRNOTAVAIL)) => {
            listen(Domain::IPV4, SocketAddr::from((Ipv4Addr::UNSPECIFIED, port)), false)
        }
        other => other,
    }
}

fn listen(domain: Domain, addr: SocketAddr, dual_stack: bool) -> io::Result<std::net::TcpListener> {
    let socket = Socket::new(domain, Type::STREAM, None)?;
    if dual_stack {
        socket.set_only_v6(false)?;
    }
    socket.set_reuse_address(true)?;
    socket.bind(&addr.into())?;
    socket.listen(1024)?;
    socket.set_nonblocking(true)?;
    Ok(socket.into())
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .init();
    let args = Args::parse_from(go_style_args());
    let config = Config::load(&args.config).with_context(|| format!("reading {}", args.config.display()))?;
    if args.claude_login {
        let options = cpa_exec::claude_login::LoginOptions {
            no_browser: args.no_browser,
            callback_port: match args.oauth_callback_port {
                0 => cpa_exec::claude_login::DEFAULT_CALLBACK_PORT,
                port => port,
            },
        };
        if let Err(error) = cpa_exec::claude_login::login(&config.auth_dir, &options).await {
            let message = String::from_utf8_lossy(&error.body).into_owned();
            // DoClaudeLogin: a busy callback port exits with ErrPortInUse's code 13.
            if message.starts_with(cpa_exec::claude_login::PORT_IN_USE) {
                tracing::error!(
                    "The required port is already in use. Please close any applications using port 3000 and try again."
                );
                std::process::exit(13);
            }
            println!("Claude authentication failed: {message}");
        }
        return Ok(());
    }
    if args.codex_login || args.codex_device_login {
        let options = cpa_exec::codex_oauth::LoginOptions {
            no_browser: args.no_browser,
            callback_port: match args.oauth_callback_port {
                0 => cpa_exec::codex_oauth::DEFAULT_CALLBACK_PORT,
                port => port,
            },
        };
        let path = if args.codex_login {
            cpa_exec::codex_oauth::login(&config.auth_dir, &options).await?
        } else {
            cpa_exec::codex_oauth::device_login(&config.auth_dir, &options).await?
        };
        println!("Authentication saved to {}", path.display());
        println!("Codex authentication successful!");
        return Ok(());
    }
    if args.kimi_login || args.kimi_ai_login {
        let provider = if args.kimi_login { "kimi" } else { "kimi-ai" };
        cpa_exec::kimi_auth::login(provider, &config, args.no_browser).await?;
        return Ok(());
    }
    if args.xai_login {
        // Go DoXAILogin: a failure is logged and the command still exits normally.
        if let Err(error) = cpa_exec::xai_auth::login(&config, args.no_browser).await {
            tracing::error!("xAI authentication failed: {}", String::from_utf8_lossy(&error.body));
        }
        return Ok(());
    }
    if args.meta_login {
        cpa_exec::meta_auth::login(&config, args.no_browser).await?;
        return Ok(());
    }
    if config.api_keys.is_empty() {
        tracing::warn!("access.api-keys is empty: the proxy API is open to anyone who can reach it");
    }
    // Auth-dir files and config API keys, synthesized as Go's watcher does.
    let credentials = cpa_core::config::credentials::load(&config);
    tracing::info!(credentials = credentials.len(), "credentials loaded");
    let listener =
        bind(&config.host, config.port).with_context(|| format!("binding {}:{}", config.host, config.port))?;
    let listener = tokio::net::TcpListener::from_std(listener)?;
    tracing::info!(addr = %listener.local_addr()?, "listening");
    let executors = Executors {
        claude: ClaudeExecutor::new(DEFAULT_BASE_URL)?,
        codex: cpa_exec::codex::CodexExecutor::new()?,
        devices: Default::default(),
        openai: Default::default(),
        google: Default::default(),
    };
    let rt = Arc::new(Runtime::new(config, credentials, executors));
    // Translators and thinking validation read model capabilities through the global
    // overlay; without it they see only the static catalog.
    cpa_server::install_registry(&rt);
    rt.start_auto_refresh();
    let options = cpa_server::management::Options {
        local_password: args.password,
        ..Default::default()
    };
    let management = cpa_server::management::Management::with_options(rt.clone(), args.config, options);
    let _watcher = cpa_server::watching::start(&management);
    // Go applies its CORS middleware to every route, not only management.
    let app = router(rt)
        .merge(cpa_server::management::router(management))
        .layer(axum::middleware::from_fn(cpa_server::management::cors));
    axum::serve(listener, app.into_make_service_with_connect_info::<SocketAddr>()).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn explicit_host_listener_is_nonblocking_for_tokio() {
        let listener = bind("127.0.0.1", 0).unwrap();
        let port = listener.local_addr().unwrap().port();
        let listener = tokio::net::TcpListener::from_std(listener).unwrap();
        let client = tokio::net::TcpStream::connect((Ipv4Addr::LOCALHOST, port));
        let (client, accepted) = tokio::join!(client, listener.accept());
        assert!(client.is_ok() && accepted.is_ok());
    }

    #[test]
    fn empty_host_is_dual_stack_and_port_in_use_is_an_error() {
        let listener = bind("", 0).unwrap();
        let port = listener.local_addr().unwrap().port();
        if listener.local_addr().unwrap().is_ipv6() {
            std::net::TcpStream::connect((Ipv4Addr::LOCALHOST, port)).expect("IPv4 reaches the [::] listener");
            std::net::TcpStream::connect((Ipv6Addr::LOCALHOST, port)).expect("IPv6 reaches the [::] listener");
        }
        let err = bind("", port).unwrap_err();
        assert_eq!(
            err.raw_os_error(),
            Some(libc::EADDRINUSE),
            "must not silently fall back to IPv4"
        );
    }
}
