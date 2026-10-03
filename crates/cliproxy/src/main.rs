mod discovery;
mod dotenv;

use std::io;
use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use clap::{ArgAction, Parser};
use cpa_core::config::Config;
use cpa_exec::Executors;
use cpa_exec::claude::{ClaudeExecutor, DEFAULT_BASE_URL};
use cpa_server::{Runtime, router};
use socket2::{Domain, Socket, Type};

/// Same strings as the management `X-CPA-*` headers.
const VERSION: &str = concat!("cliproxy-rs-", env!("CARGO_PKG_VERSION"));
const COMMIT: &str = match option_env!("CPA_COMMIT") {
    Some(v) => v,
    None => "none",
};
const BUILD_DATE: &str = match option_env!("CPA_BUILD_DATE") {
    Some(v) => v,
    None => "unknown",
};

fn banner() -> String {
    format!("CLIProxyAPI Version: {VERSION}, Commit: {COMMIT}, BuiltAt: {BUILD_DATE}")
}

/// Flags mirror CLIProxyAPI. Go's single-dash long flags (`-config`) are accepted.
#[derive(Parser)]
#[command(version, about)]
struct Args {
    /// Configure File Path
    #[arg(long, default_value = "")]
    config: String,
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
    /// Login to Antigravity using OAuth
    #[arg(long)]
    antigravity_login: bool,
    /// Login to Kimi (.com) using OAuth
    #[arg(long)]
    kimi_login: bool,
    /// Login to Kimi.ai using OAuth
    #[arg(long)]
    kimi_ai_login: bool,
    /// Login to xAI using OAuth
    #[arg(long)]
    xai_login: bool,
    /// Login to Devin using OAuth
    #[arg(long)]
    devin_login: bool,
    /// Login to Meta using OAuth
    #[arg(long)]
    meta_login: bool,
    /// Discover local AI gateways and CPA instances on the LAN
    #[arg(long)]
    discover: bool,
    /// Timeout in seconds for LAN discovery (default 3s)
    #[arg(long, default_value_t = 3, allow_negative_numbers = true)]
    discover_timeout: i64,
    /// Output discovered gateways in JSON format
    #[arg(long)]
    discover_json: bool,
    /// DNS-SD service type for LAN discovery (default _ai-gateway._tcp)
    #[arg(long, default_value = "")]
    discover_service_type: String,
    /// Comma-separated interface names to scan during LAN discovery
    #[arg(long, action = ArgAction::Append)]
    discover_include: Vec<String>,
    /// Comma-separated interface names to skip during LAN discovery
    #[arg(long, action = ArgAction::Append)]
    discover_exclude: Vec<String>,
    /// Import Vertex service account key JSON file
    #[arg(long, default_value = "")]
    vertex_import: String,
    /// Prefix for Vertex model namespacing (use with -vertex-import)
    #[arg(long, default_value = "")]
    vertex_import_prefix: String,
    /// Management password accepted from loopback clients only.
    #[arg(long, hide = true, default_value = "")]
    password: String,
    /// Home control plane JWT for mTLS certificate bootstrap and connection
    #[arg(long, default_value = "")]
    home_jwt: String,
    /// Disable Home CLUSTER NODES discovery and keep using the configured -home-jwt address
    #[arg(long)]
    home_disable_cluster_discovery: bool,
    /// Start with terminal management UI
    #[arg(long)]
    tui: bool,
    /// In TUI mode, start an embedded local server
    #[arg(long)]
    standalone: bool,
    /// Base URL of remote management API for TUI client mode (e.g. https://proxy.example.com)
    #[arg(long, default_value = "")]
    management_base_url: String,
    /// Use embedded models.json and codex_client_models.json only, skip remote model catalog fetching
    #[arg(long)]
    local_model: bool,
}

/// Go's `discover` subcommand flag set.
#[derive(Parser)]
#[command(name = "discover")]
struct DiscoverArgs {
    /// Discovery timeout in seconds
    #[arg(long, default_value_t = 3, allow_negative_numbers = true)]
    timeout: i64,
    /// Output in JSON format
    #[arg(long)]
    json: bool,
    /// DNS-SD service type (default _ai-gateway._tcp)
    #[arg(long, default_value = "")]
    service_type: String,
    /// Configure File Path
    #[arg(long, default_value = "")]
    config: String,
    /// Comma-separated interface names to scan (overrides default physical LAN filter)
    #[arg(long, action = ArgAction::Append)]
    include: Vec<String>,
    /// Comma-separated interface names to skip
    #[arg(long, action = ArgAction::Append)]
    exclude: Vec<String>,
}

/// Go's flag package treats `-name` and `--name` the same; clap needs `--name`.
fn go_style_args(args: impl IntoIterator<Item = String>) -> Vec<String> {
    args.into_iter()
        .enumerate()
        .map(|(i, a)| {
            let single_dash_long = i > 0 && a.len() > 2 && a.starts_with('-') && !a.starts_with("--");
            if single_dash_long { format!("-{a}") } else { a }
        })
        .collect()
}

/// Go `strconv.ParseBool`.
fn go_parse_bool(v: &str) -> Option<bool> {
    match v {
        "1" | "t" | "T" | "TRUE" | "true" | "True" => Some(true),
        "0" | "f" | "F" | "FALSE" | "false" | "False" => Some(false),
        _ => None,
    }
}

/// Go `argvEnablesBoolFlag`: a pre-parse scan (stops at the first non-flag) used to
/// keep stdout clean for `--discover-json`.
fn argv_enables_bool_flag(args: &[String], name: &str) -> bool {
    const BOOL_FLAGS: [&str; 16] = [
        "codex-login",
        "codex-device-login",
        "claude-login",
        "no-browser",
        "antigravity-login",
        "kimi-login",
        "kimi-ai-login",
        "xai-login",
        "devin-login",
        "meta-login",
        "discover",
        "discover-json",
        "home-disable-cluster-discovery",
        "tui",
        "standalone",
        "local-model",
    ];
    let mut enabled = false;
    let mut i = 0;
    while i < args.len() {
        let arg = &args[i];
        if arg == "--" || arg == "-" || !arg.starts_with('-') {
            break;
        }
        let bare = arg.strip_prefix('-').unwrap_or(arg);
        let bare = bare.strip_prefix('-').unwrap_or(bare);
        let (flag, value) = match bare.split_once('=') {
            Some((f, v)) => (f, Some(v)),
            None => (bare, None),
        };
        if flag == name {
            match value {
                None => enabled = true,
                Some(v) => enabled = go_parse_bool(v).unwrap_or(enabled),
            }
        }
        if value.is_none()
            && !flag.is_empty()
            && !BOOL_FLAGS.contains(&flag)
            && args.get(i + 1).is_some_and(|n| n != "--")
        {
            i += 1;
        }
        i += 1;
    }
    enabled
}

/// Go's `appendCSV` flag function: each occurrence is split and appended.
fn csv_flags(values: &[String]) -> Vec<String> {
    values
        .iter()
        .flat_map(|v| discovery::cli::parse_interface_list(std::slice::from_ref(v)))
        .collect()
}

fn discover_options(
    timeout: i64,
    json: bool,
    service_type: String,
    config: &str,
    cli: (Vec<String>, Vec<String>),
) -> discovery::cli::Options {
    let (include, exclude) = discovery::cli::resolve_filters(cli, discovery::cli::load_scan_filters(config));
    discovery::cli::Options {
        timeout: Duration::from_secs(timeout.clamp(0, 3600) as u64),
        json,
        service_type,
        include,
        exclude,
    }
}

fn runtime() -> io::Result<tokio::runtime::Runtime> {
    tokio::runtime::Builder::new_multi_thread().enable_all().build()
}

/// Waits for Ctrl+C or SIGTERM, Go's `signal.NotifyContext(SIGINT, SIGTERM)`.
async fn shutdown_signal() {
    #[cfg(unix)]
    {
        let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("SIGTERM handler installs");
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = term.recv() => {}
        }
    }
    #[cfg(not(unix))]
    let _ = tokio::signal::ctrl_c().await;
}

/// Go `LoadConfigOptional`: in cloud deploy mode a missing, empty or unparsable file
/// is an empty config (the server then stands by).
fn load_config(path: &Path, optional: bool) -> anyhow::Result<Config> {
    if optional {
        let empty = match std::fs::read(path) {
            Err(e) => matches!(e.kind(), io::ErrorKind::NotFound | io::ErrorKind::IsADirectory),
            Ok(bytes) => {
                String::from_utf8_lossy(&bytes).trim().is_empty()
                    || serde_yaml_ng::from_slice::<serde_yaml_ng::Value>(&bytes).is_err()
            }
        };
        if empty {
            return Config::parse("");
        }
    }
    Config::load(path).with_context(|| format!("failed to load config {}", path.display()))
}

/// Go's cloud-deploy check of the config file (main.go, `isCloudDeploy`).
fn cloud_config_present(path: &Path, config: &Config) -> bool {
    let (present, message) = match std::fs::metadata(path) {
        Err(_) => (
            false,
            "Cloud deploy mode: No configuration file detected; standing by for configuration",
        ),
        Ok(m) if m.is_dir() => (
            false,
            "Cloud deploy mode: Config path is a directory; standing by for configuration",
        ),
        Ok(_) if config.port == 0 => (
            false,
            "Cloud deploy mode: Configuration file is empty or invalid; standing by for valid configuration",
        ),
        Ok(_) => (true, "Cloud deploy mode: Configuration file detected; starting service"),
    };
    tracing::info!("{message}");
    present
}

/// Go `cmd.WaitForCloudDeploy`.
async fn wait_for_cloud_deploy() {
    tracing::info!(
        "Cloud deploy mode: No config found; standing by for configuration. API server is not started. Press Ctrl+C to exit."
    );
    shutdown_signal().await;
    tracing::info!("Cloud deploy mode: Shutdown signal received; exiting");
}

fn unsupported(what: &str) -> ! {
    // ponytail: these Go modes have no Rust port yet; refuse instead of starting a server.
    tracing::error!("{what} is not supported by cliproxy-rs yet");
    std::process::exit(1);
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

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .init();
    let raw: Vec<String> = std::env::args().collect();
    if raw.get(1).map(String::as_str) == Some("discover") {
        let args = DiscoverArgs::parse_from(go_style_args(raw.into_iter().skip(1)));
        if !args.json {
            eprintln!("{}", banner());
        }
        let cli = (csv_flags(&args.include), csv_flags(&args.exclude));
        let opts = discover_options(args.timeout, args.json, args.service_type, &args.config, cli);
        std::process::exit(runtime()?.block_on(discovery::cli::discover(&opts)));
    }
    if !argv_enables_bool_flag(&raw[1..], "discover-json") {
        println!("{}", banner());
    }
    let args = Args::parse_from(go_style_args(raw));
    if args.discover || args.discover_json {
        let cli = (csv_flags(&args.discover_include), csv_flags(&args.discover_exclude));
        let opts = discover_options(
            args.discover_timeout,
            args.discover_json,
            args.discover_service_type.clone(),
            &args.config,
            cli,
        );
        std::process::exit(runtime()?.block_on(discovery::cli::discover(&opts)));
    }
    // Before the runtime starts: setting variables is only sound single-threaded.
    dotenv::load_from_working_dir();
    runtime()?.block_on(run(args))
}

async fn run(args: Args) -> anyhow::Result<()> {
    let config_path = if args.config.is_empty() {
        std::env::current_dir()?.join("config.yaml")
    } else {
        PathBuf::from(&args.config)
    };
    let cloud = std::env::var("DEPLOY").is_ok_and(|v| v == "cloud");
    let config = load_config(&config_path, cloud)?;
    let config_present = !cloud || cloud_config_present(&config_path, &config);
    tracing::info!("{}", banner());
    let home_jwt = [
        args.home_jwt.clone(),
        std::env::var("HOME_JWT").unwrap_or_default(),
        std::env::var("home_jwt").unwrap_or_default(),
    ];
    if home_jwt.iter().any(|v| !v.trim().is_empty()) {
        unsupported("Home control plane mode (-home-jwt)");
    }
    // Command modes, in Go's order.
    if !args.vertex_import.is_empty() {
        unsupported("Vertex service account import (-vertex-import)");
    }
    if args.antigravity_login {
        unsupported("Antigravity login (-antigravity-login)");
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
    if args.devin_login {
        unsupported("Devin login (-devin-login)");
    }
    if args.meta_login {
        cpa_exec::meta_auth::login(&config, args.no_browser).await?;
        return Ok(());
    }
    if !config_present {
        wait_for_cloud_deploy().await;
        return Ok(());
    }
    if args.local_model && (!args.tui || args.standalone) {
        tracing::info!("Local model mode: using embedded model catalogs, remote model updates disabled");
    }
    if args.tui {
        eprintln!("TUI error: the terminal UI is not available in this build yet");
        return Ok(());
    }
    serve(config, config_path, args.password, args.local_model).await
}

async fn serve(config: Config, config_path: PathBuf, password: String, local_model: bool) -> anyhow::Result<()> {
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
    rt.set_local_model(local_model);
    // Translators and thinking validation read model capabilities through the global
    // overlay; without it they see only the static catalog.
    cpa_server::install_registry(&rt);
    rt.start_auto_refresh();
    let options = cpa_server::management::Options {
        local_password: password,
        ..Default::default()
    };
    let management = cpa_server::management::Management::with_options(rt.clone(), config_path, options);
    let _watcher = cpa_server::watching::start(&management);
    let advertiser = {
        let rt = rt.clone();
        discovery::advertise::Advertiser::spawn(move || rt.config())
    };
    // Go applies its CORS middleware to every route, not only management.
    let app = router(rt)
        .merge(cpa_server::management::router(management))
        .layer(axum::middleware::from_fn(cpa_server::management::cors));
    let server = axum::serve(listener, app.into_make_service_with_connect_info::<SocketAddr>());
    // ponytail: no HTTP drain on shutdown; Go drains with a deadline. The signal ends
    // the process after the mDNS goodbye, as before minus the signal exit status.
    tokio::select! {
        r = server => r?,
        _ = shutdown_signal() => {}
    }
    advertiser.shutdown().await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn argv_prescan_matches_go() {
        let fixture: serde_json::Value =
            serde_json::from_str(include_str!("../tests/fixtures/discovery_main_go.json")).unwrap();
        for case in fixture["argv"].as_array().unwrap() {
            let args: Vec<String> = serde_json::from_value(case["args"].clone()).unwrap();
            assert_eq!(
                argv_enables_bool_flag(&args, "discover-json"),
                case["discover_json"],
                "{args:?}"
            );
            assert_eq!(argv_enables_bool_flag(&args, "discover"), case["discover"], "{args:?}");
        }
    }

    #[test]
    fn every_go_flag_parses_in_single_and_double_dash_form() {
        let argv = |a: &[&str]| go_style_args(std::iter::once("cliproxy").chain(a.iter().copied()).map(String::from));
        let args = Args::try_parse_from(argv(&[
            "-discover",
            "-discover-timeout",
            "-2",
            "--discover-json",
            "-discover-service-type=_x._tcp",
            "-discover-include",
            "eth0,en0",
            "-discover-include=wl*",
            "-discover-exclude",
            "docker0",
            "-vertex-import",
            "k.json",
            "-vertex-import-prefix",
            "p",
            "-home-jwt",
            "j",
            "-home-disable-cluster-discovery",
            "-tui",
            "-standalone",
            "-management-base-url",
            "https://x",
            "-local-model",
            "-antigravity-login",
            "-devin-login",
            "-config",
            "c.yaml",
            "-password",
            "pw",
        ]))
        .unwrap();
        assert!(args.discover && args.discover_json && args.tui && args.standalone && args.local_model);
        assert!(args.antigravity_login && args.devin_login && args.home_disable_cluster_discovery);
        assert_eq!(args.discover_timeout, -2);
        assert_eq!(csv_flags(&args.discover_include), ["eth0", "en0", "wl*"]);
        assert_eq!(args.management_base_url, "https://x");
        let sub = DiscoverArgs::try_parse_from(go_style_args(
            [
                "discover", "-timeout", "5", "-json", "-include", "a,b", "-include", "a", "-config", "x.yaml",
            ]
            .map(String::from),
        ))
        .unwrap();
        assert_eq!(
            (sub.timeout, sub.json, csv_flags(&sub.include)),
            (5, true, vec!["a".into(), "b".into(), "a".into()])
        );
    }

    #[test]
    fn cloud_mode_treats_missing_empty_and_broken_files_as_empty_config() {
        let dir = std::env::temp_dir().join(format!("cpa-cloud-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let missing = dir.join("missing.yaml");
        assert_eq!(load_config(&missing, true).unwrap().port, 0);
        assert!(load_config(&missing, false).is_err());
        assert_eq!(load_config(&dir, true).unwrap().port, 0, "a directory is standby too");
        let broken = dir.join("broken.yaml");
        std::fs::write(&broken, "port: [\n").unwrap();
        assert_eq!(load_config(&broken, true).unwrap().port, 0);
        assert!(load_config(&broken, false).is_err());
        let good = dir.join("good.yaml");
        std::fs::write(&good, "port: 9\n").unwrap();
        let cfg = load_config(&good, true).unwrap();
        assert!(cloud_config_present(&good, &cfg));
        assert!(!cloud_config_present(&missing, &Config::parse("").unwrap()));
        assert!(!cloud_config_present(&dir, &Config::parse("").unwrap()));
        let _ = std::fs::rename(
            &dir,
            std::env::temp_dir().join(format!("cpa-trash-cloud-{}", std::process::id())),
        );
    }

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
