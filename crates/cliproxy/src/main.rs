mod discovery;
mod dotenv;
mod home;
mod plugin_cli;
mod tui;

use std::io;
use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use clap::{ArgAction, CommandFactory, FromArgMatches, Parser};
use cpa_core::config::Config;
use cpa_exec::Executors;
use cpa_exec::claude::{ClaudeExecutor, DEFAULT_BASE_URL};
use cpa_server::Runtime;
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
    /// Write process logs to this file (10 MiB rotation), without a shell wrapper.
    #[arg(long)]
    log_file: Option<PathBuf>,
    /// Change to this directory first, before reading .env, plugins or relative paths.
    #[arg(long)]
    working_dir: Option<PathBuf>,
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
    #[arg(long, default_value_t = 3, value_parser = go_int)]
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
    #[arg(long, default_value_t = 3, value_parser = go_int)]
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

/// Rewrites a command line parsed by Go's `flag` package into clap's form. Go decides
/// which token is a value (a non-boolean flag takes the next token even when it starts
/// with `-`), booleans accept `=value`, a repeated scalar keeps its last value, and
/// parsing stops at the first operand or `--`; Go ignores everything after that.
/// Unknown or malformed flags are passed on for clap to report. Plugin flags are set
/// on `plugins` as they are parsed; a value they reject is Go's parse error.
fn go_flags(
    cmd: &clap::Command,
    args: impl IntoIterator<Item = String>,
    plugins: &dyn plugin_cli::PluginFlags,
) -> Result<Vec<String>, String> {
    let mut args = args.into_iter();
    let mut out: Vec<String> = args.next().into_iter().collect();
    let args: Vec<String> = args.collect();
    let mut bools: Vec<(String, bool)> = Vec::new();
    let mut scalars: Vec<(String, String)> = Vec::new();
    let mut appended: Vec<String> = Vec::new();
    let mut tail = None;
    let mut i = 0;
    while i < args.len() {
        let arg = &args[i];
        i += 1;
        if arg == "--" || arg.len() < 2 || !arg.starts_with('-') {
            break;
        }
        let bare = arg.strip_prefix("--").unwrap_or(&arg[1..]);
        let (name, value) = match bare.split_once('=') {
            Some((n, v)) => (n, Some(v)),
            None => (bare, None),
        };
        // Go's FlagSet holds the plugin flags beside the built-in ones (names never
        // clash: registration skips taken names).
        if let Some(is_bool) = plugins
            .lookup(name)
            .filter(|_| !name.is_empty() && !name.starts_with(['-', '=']))
        {
            let quote = cpa_common::gostr::quote;
            if is_bool {
                match value {
                    None => plugins
                        .set(name, "true")
                        .map_err(|e| format!("invalid boolean flag {name}: {e}"))?,
                    Some(v) => plugins
                        .set(name, v)
                        .map_err(|e| format!("invalid boolean value {} for -{name}: {e}", quote(v)))?,
                }
                continue;
            }
            let value = match value {
                Some(v) => v.to_owned(),
                None if i < args.len() => {
                    i += 1;
                    args[i - 1].clone()
                }
                None => return Err(format!("flag needs an argument: -{name}")),
            };
            plugins
                .set(name, &value)
                .map_err(|e| format!("invalid value {} for flag -{name}: {e}", quote(&value)))?;
            continue;
        }
        let known = cmd.get_arguments().find(|a| a.get_long() == Some(name));
        let Some(flag) = known.filter(|_| !name.is_empty() && !name.starts_with(['-', '='])) else {
            tail = Some(if matches!(name, "h" | "help") {
                "--help".to_owned()
            } else {
                format!("--{bare}")
            });
            break;
        };
        if !flag.get_action().takes_values() {
            match value.map(go_parse_bool) {
                None => upsert(&mut bools, name, true),
                Some(Some(b)) => upsert(&mut bools, name, b),
                Some(None) => {
                    tail = Some(format!("--{bare}"));
                    break;
                }
            }
            continue;
        }
        let value = match value {
            Some(v) => v.to_owned(),
            None if i < args.len() => {
                i += 1;
                args[i - 1].clone()
            }
            None => {
                tail = Some(format!("--{name}"));
                break;
            }
        };
        if matches!(flag.get_action(), ArgAction::Append) {
            appended.push(format!("--{name}={value}"));
        } else {
            upsert(&mut scalars, name, value);
        }
    }
    out.extend(bools.into_iter().filter(|(_, on)| *on).map(|(n, _)| format!("--{n}")));
    out.extend(scalars.into_iter().map(|(n, v)| format!("--{n}={v}")));
    out.extend(appended);
    out.extend(tail);
    Ok(out)
}

fn upsert<T>(list: &mut Vec<(String, T)>, name: &str, value: T) {
    match list.iter_mut().find(|(n, _)| n == name) {
        Some(slot) => slot.1 = value,
        None => list.push((name.to_owned(), value)),
    }
}

/// Go `strconv.ParseInt(s, 0, 64)`, as `flag.Int` parses: sign, `0x`/`0o`/`0b`/`0`
/// prefixes and digit-separating underscores.
fn go_int(s: &str) -> Result<i64, String> {
    let err = || format!("invalid value {s:?}: parse error");
    let (neg, digits) = match s.as_bytes().first() {
        Some(b'-') => (true, &s[1..]),
        Some(b'+') => (false, &s[1..]),
        _ => (false, s),
    };
    let lower = digits.to_ascii_lowercase();
    // A base prefix (or the leading 0 of an octal literal) counts as a digit before an
    // underscore, as in strconv's underscoreOK.
    let (radix, body, prefixed) = match lower.as_bytes() {
        [b'0', b'x', ..] => (16, &lower[2..], true),
        [b'0', b'o', ..] => (8, &lower[2..], true),
        [b'0', b'b', ..] => (2, &lower[2..], true),
        [b'0', _, ..] => (8, &lower[1..], true),
        _ => (10, lower.as_str(), false),
    };
    if body.is_empty() || (body.starts_with('_') && !prefixed) || body.ends_with('_') || body.contains("__") {
        return Err(err());
    }
    let magnitude = i128::from_str_radix(&body.replace('_', ""), radix).map_err(|_| err())?;
    i64::try_from(if neg { -magnitude } else { magnitude }).map_err(|_| err())
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
            && args
                .get(i + 1)
                .is_some_and(|n| n != "--" && (builtin_flag(flag) || !is_discover_flag(n)))
        {
            i += 1;
        }
        i += 1;
    }
    enabled
}

/// The value of the built-in flag `wanted` (`-flag V`, `--flag V`, `-flag=V`,
/// `--flag=V`; the last one wins), read from the raw arguments for what must happen
/// before the parse: `-working-dir` changes the directory before `.env`, the plugin
/// bootstrap or any relative path is read, and `-log-file` opens before the runtime
/// starts. Like Go's parse it stops at the first operand or `--`, and built-in value
/// flags take the next token. A flag the binary does not define may be a plugin flag:
/// it is read as taking the next token unless that token is a flag; the full parse
/// confirms the result.
fn prescan_value(args: &[String], wanted: &str) -> Option<String> {
    let mut found = None;
    let mut i = 0;
    while i < args.len() {
        let arg = &args[i];
        i += 1;
        if arg == "--" || arg.len() < 2 || !arg.starts_with('-') {
            break;
        }
        let bare = arg.strip_prefix("--").unwrap_or(&arg[1..]);
        let (name, value) = match bare.split_once('=') {
            Some((n, v)) => (n, Some(v)),
            None => (bare, None),
        };
        if value.is_some() {
            if name == wanted {
                found = value.map(str::to_owned);
            }
            continue;
        }
        let Some(next) = args.get(i) else { break };
        let takes_value = match Args::command().get_arguments().find(|a| a.get_long() == Some(name)) {
            Some(flag) => flag.get_action().takes_values(),
            None => !next.starts_with('-'),
        };
        if takes_value {
            if name == wanted {
                found = Some(next.clone());
            }
            i += 1;
        }
    }
    found
}

/// Whether the binary itself defines `name` (plugin flags are only known once the
/// plugins have loaded).
fn builtin_flag(name: &str) -> bool {
    static NAMES: std::sync::LazyLock<Vec<String>> = std::sync::LazyLock::new(|| {
        Args::command()
            .get_arguments()
            .filter_map(|a| a.get_long().map(str::to_owned))
            .collect()
    });
    NAMES.iter().any(|n| n == name)
}

/// `-discover` or `-discover-json`, with one or two dashes and an optional value.
fn is_discover_flag(arg: &str) -> bool {
    let Some(bare) = arg.strip_prefix('-') else {
        return false;
    };
    let bare = bare.strip_prefix('-').unwrap_or(bare);
    let flag = bare.split_once('=').map_or(bare, |(f, _)| f);
    matches!(flag, "discover" | "discover-json")
}

/// In discover mode no plugin loads, so the parse accepts exactly the flags the
/// pre-scan read as boolean plugin flags (an undefined flag right before
/// `-discover` or `-discover-json`) and ignores them.
struct PrescanBools(Vec<String>);

impl PrescanBools {
    fn from_args(args: &[String]) -> Self {
        let names = args
            .iter()
            .zip(args.iter().skip(1))
            .take_while(|(arg, _)| *arg != "--")
            .filter(|(_, next)| is_discover_flag(next))
            .filter_map(|(arg, _)| {
                let bare = arg.strip_prefix('-')?;
                let bare = bare.strip_prefix('-').unwrap_or(bare);
                (!bare.is_empty() && !bare.contains('=') && !builtin_flag(bare)).then(|| bare.to_owned())
            })
            .collect();
        Self(names)
    }
}

impl plugin_cli::PluginFlags for PrescanBools {
    fn lookup(&self, name: &str) -> Option<bool> {
        self.0.iter().any(|n| n == name).then_some(true)
    }
    fn set(&self, _: &str, _: &str) -> Result<(), String> {
        Ok(())
    }
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

/// A personal proxy spends its time waiting on upstreams, so two workers serve hundreds
/// of requests per second, and each extra worker can hold its own glibc arena. The
/// blocking pool keeps tokio's default ceiling (512): its threads start on demand and
/// exit after 10 idle seconds, and a native plugin call holds one for the whole call
/// while its HTTP callbacks need others (DNS), so a low ceiling could deadlock.
fn runtime(config: Option<&Path>) -> io::Result<tokio::runtime::Runtime> {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(worker_threads(config))
        .enable_all()
        .build()
}

/// `worker-threads` as written in `path`, if any.
fn configured_workers(path: &Path) -> Option<u64> {
    let raw = std::fs::read(path).ok()?;
    let doc = serde_yaml_ng::from_slice::<serde_yaml_ng::Value>(&raw).ok()?;
    doc.get("worker-threads")?.as_u64()
}

/// `TOKIO_WORKER_THREADS`, else `worker-threads` in `config` (read once at start), else
/// the smaller of the CPU count and 2.
fn worker_threads(config: Option<&Path>) -> usize {
    let positive = |n: u64| usize::try_from(n).ok().filter(|n| *n > 0);
    if let Ok(raw) = std::env::var("TOKIO_WORKER_THREADS") {
        match raw.trim().parse().ok().and_then(positive) {
            Some(n) => return n,
            None => tracing::warn!("ignoring TOKIO_WORKER_THREADS={raw:?}: not a positive number"),
        }
    }
    let configured = config.and_then(configured_workers).and_then(positive);
    configured.unwrap_or_else(|| std::thread::available_parallelism().map_or(1, |n| n.get().min(2)))
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

/// Go `LoadConfigOptional`: in cloud deploy mode a missing, empty or undecodable file
/// is an empty config (the server then stands by).
fn load_config(path: &Path, optional: bool) -> anyhow::Result<Config> {
    let loaded = Config::load(path).with_context(|| format!("failed to load config {}", path.display()));
    match loaded {
        Err(e) if optional => {
            let missing = std::fs::read(path).map_or(true, |b| String::from_utf8_lossy(&b).trim().is_empty());
            if !missing {
                // ponytail: Go stays fatal for its post-decode validations (trusted
                // proxies, weights, in-flight); cpa-core reports both kinds alike, so
                // cloud mode stands by on every load error and logs it.
                tracing::error!("{e:#}");
            }
            Config::parse("")
        }
        other => other,
    }
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
        Err(e) if no_ipv6(&e) => listen(Domain::IPV4, SocketAddr::from((Ipv4Addr::UNSPECIFIED, port)), false),
        other => other,
    }
}

/// The host has no usable IPv6: the address family is unsupported or `::` cannot be
/// assigned. Windows reports Winsock codes, not the C runtime's errno values.
fn no_ipv6(e: &io::Error) -> bool {
    #[cfg(not(windows))]
    const EAFNOSUPPORT: i32 = libc::EAFNOSUPPORT;
    #[cfg(windows)]
    const EAFNOSUPPORT: i32 = 10047; // WSAEAFNOSUPPORT
    e.kind() == io::ErrorKind::AddrNotAvailable || e.raw_os_error() == Some(EAFNOSUPPORT)
}

fn listen(domain: Domain, addr: SocketAddr, dual_stack: bool) -> io::Result<std::net::TcpListener> {
    let socket = Socket::new(domain, Type::STREAM, None)?;
    if dual_stack {
        socket.set_only_v6(false)?;
    }
    // Go sets SO_REUSEADDR except on Windows, where it would let another socket bind a
    // port already in use (net/sockopt_windows.go).
    #[cfg(not(windows))]
    socket.set_reuse_address(true)?;
    socket.bind(&addr.into())?;
    socket.listen(1024)?;
    socket.set_nonblocking(true)?;
    Ok(socket.into())
}

/// Go `flag.ExitOnError` after a parse error: the message, the usage, exit 2.
fn flag_error(cmd: &mut clap::Command, message: &str) -> ! {
    eprintln!("{message}");
    eprint!("{}", cmd.render_help());
    std::process::exit(2);
}

fn main() -> anyhow::Result<()> {
    cpa_server::logging::init();
    let raw: Vec<String> = std::env::args().collect();
    // First, so .env, plugins, the default config and relative paths resolve there.
    let working_dir = prescan_value(&raw[1..], "working-dir");
    if let Some(dir) = &working_dir {
        std::env::set_current_dir(dir).with_context(|| format!("change to working directory {dir:?}"))?;
    }
    // Next, relative to that directory, so every line from here on reaches the file:
    // sizing the runtime can warn, and stdout may be hidden (the Windows Run entry).
    // An empty value is left for the parse to reject.
    let log_file = prescan_value(&raw[1..], "log-file");
    if let Some(path) = log_file.as_deref().filter(|p| !p.is_empty()) {
        cpa_server::logging::set_log_file(path.into()).context("open process log")?;
    }
    if raw.get(1).map(String::as_str) == Some("discover") {
        let flags = go_flags(&DiscoverArgs::command(), raw.into_iter().skip(1), &())
            .unwrap_or_else(|e| flag_error(&mut DiscoverArgs::command(), &e));
        let args = DiscoverArgs::parse_from(flags);
        if !args.json {
            eprintln!("{}", banner());
        }
        let cli = (csv_flags(&args.include), csv_flags(&args.exclude));
        let opts = discover_options(args.timeout, args.json, args.service_type, &args.config, cli);
        std::process::exit(runtime(None)?.block_on(discovery::cli::discover(&opts)));
    }
    if !argv_enables_bool_flag(&raw[1..], "discover-json") {
        println!("{}", banner());
    }
    // Go `isDiscoverMode`: discovery neither loads plugins nor reads .env.
    let discover_mode =
        argv_enables_bool_flag(&raw[1..], "discover-json") || argv_enables_bool_flag(&raw[1..], "discover");
    // Before the runtime starts: setting variables is only sound single-threaded.
    // ponytail: Go reads .env after the flag parse, so plugins loaded for their flags do
    // not see it; here it is read before they load (they do).
    if !discover_mode {
        dotenv::load_from_working_dir();
    }
    let runtime = runtime(Some(&plugin_cli::bootstrap_config_path(&raw[1..], "")))?;
    // Go: plugins from the bootstrap config register their flags before the parse.
    let builtin_cmd = Args::command();
    let plugins = (!discover_mode).then(|| {
        let builtin = |name: &str| builtin_cmd.get_arguments().any(|a| a.get_long() == Some(name));
        runtime.block_on(plugin_cli::bootstrap(&raw[1..], &builtin))
    });
    let mut cmd = match &plugins {
        Some((_, flags)) => plugin_cli::with_flags(Args::command(), flags),
        None => Args::command(),
    };
    let flags = match &plugins {
        Some((host, _)) => go_flags(&cmd, raw, host),
        None => {
            let prescanned = PrescanBools::from_args(&raw[1..]);
            go_flags(&cmd, raw, &prescanned)
        }
    }
    .unwrap_or_else(|e| flag_error(&mut cmd, &e));
    let matches = cmd.clone().get_matches_from(flags);
    let args = Args::from_arg_matches(&matches).unwrap_or_else(|e| e.exit());
    if args.working_dir.as_deref() != working_dir.as_deref().map(Path::new) {
        // The pre-scan read a plugin flag's value differently from the parse.
        flag_error(
            &mut cmd,
            "-working-dir must come before any plugin flag that takes a value",
        );
    }
    if args.log_file.as_deref() != log_file.as_deref().map(Path::new) {
        flag_error(
            &mut cmd,
            "-log-file must come before any plugin flag that takes a value",
        );
    }
    if args.discover || args.discover_json {
        let cli = (csv_flags(&args.discover_include), csv_flags(&args.discover_exclude));
        let opts = discover_options(
            args.discover_timeout,
            args.discover_json,
            args.discover_service_type.clone(),
            &args.config,
            cli,
        );
        std::process::exit(runtime.block_on(discovery::cli::discover(&opts)));
    }
    let builtin = plugin_cli::builtin_values(&cmd, &matches);
    let host = plugins.map(|(host, _)| host).unwrap_or_default();
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    runtime.spawn(heap_trim::run());
    let file_log = args.log_file.is_some();
    let result = runtime.block_on(run(args, host, builtin));
    if file_log && let Err(error) = &result {
        tracing::error!("{error:#}");
    }
    result
}

/// glibc keeps the memory that request handling frees in its per-thread arenas and only
/// returns the top of each arena to the kernel, so after a burst of large prompts the
/// process stays near its peak size while most of that memory is free. `malloc_trim`
/// hands the free pages of every arena back. Trimming under load costs page faults on
/// the next requests, so it runs once the process goes quiet (under 50 ms of CPU in five
/// seconds), and at least once a minute while busy. It trims once after startup, then
/// parks until a response or a WebSocket turn ends: with no requests, no timer and no wakeup. The trim
/// itself runs on the blocking pool, off the two request threads. See
/// docs/BENCHMARKS.md (Claude soak).
///
/// Cost per request: the response body is boxed once more, and its drop does one
/// atomic load (plus a wakeup when the task is parked).
#[cfg(all(target_os = "linux", target_env = "gnu"))]
mod heap_trim {
    use std::pin::Pin;
    use std::task::{Context, Poll};
    use std::time::Duration;

    use axum::body::{Body, Bytes, HttpBody};

    const TICK: Duration = Duration::from_secs(5);
    const QUIET: Duration = Duration::from_millis(50);
    const MAX_GAP: u32 = 12;

    fn cpu_time() -> Duration {
        // SAFETY: getrusage writes only into the struct it is given.
        let mut usage: libc::rusage = unsafe { std::mem::zeroed() };
        unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut usage) };
        let tv = |t: libc::timeval| Duration::new(t.tv_sec as u64, t.tv_usec as u32 * 1000);
        tv(usage.ru_utime) + tv(usage.ru_stime)
    }

    pub async fn run() {
        loop {
            let (mut last, mut ticks) = (cpu_time(), 0);
            loop {
                tokio::time::sleep(TICK).await;
                let now = cpu_time();
                let quiet = now.saturating_sub(last) < QUIET;
                last = now;
                ticks += 1;
                if quiet || ticks >= MAX_GAP {
                    // Milliseconds to tens of ms on a large heap: not on a request thread.
                    // SAFETY: malloc_trim only releases the allocator's own free memory.
                    let _ = tokio::task::spawn_blocking(|| unsafe { libc::malloc_trim(0) }).await;
                    ticks = 0;
                }
                if quiet {
                    break;
                }
            }
            cpa_common::idle::park().await;
        }
    }

    /// Wakes the parked task when a response body is done (sent, or dropped with the
    /// connection).
    pub fn layer(app: axum::Router) -> axum::Router {
        app.layer(tower::util::MapResponseLayer::new(|res: axum::response::Response| {
            res.map(|body| Body::new(Done(body)))
        }))
    }

    struct Done(Body);

    impl HttpBody for Done {
        type Data = Bytes;
        type Error = axum::Error;

        fn poll_frame(
            self: Pin<&mut Self>,
            cx: &mut Context<'_>,
        ) -> Poll<Option<Result<http_body::Frame<Bytes>, axum::Error>>> {
            Pin::new(&mut self.get_mut().0).poll_frame(cx)
        }

        fn is_end_stream(&self) -> bool {
            self.0.is_end_stream()
        }

        fn size_hint(&self) -> http_body::SizeHint {
            self.0.size_hint()
        }
    }

    impl Drop for Done {
        fn drop(&mut self) {
            cpa_common::idle::activity();
        }
    }
}

async fn run(args: Args, plugins: cpa_plugin::Host, builtin: Vec<(String, String)>) -> anyhow::Result<()> {
    // An explicit remote URL makes `-tui` a pure management client: it needs no local
    // config, store or Home bootstrap (Go loads them first and can fail or wait there).
    // Login, import and plugin flags still run first, as in Go.
    if args.tui
        && !args.standalone
        && !args.management_base_url.trim().is_empty()
        && !command_mode(&args)
        && !plugins.has_triggered_command_line_flags()
    {
        remote_tui(args.management_base_url.trim().to_owned(), args.password).await;
        return Ok(());
    }
    let wd = std::env::current_dir()?;
    let home_jwt = [
        args.home_jwt.clone(),
        std::env::var("HOME_JWT").unwrap_or_default(),
        std::env::var("home_jwt").unwrap_or_default(),
    ];
    // Go: the flag, else HOME_JWT / home_jwt.
    let home_jwt = home_jwt
        .iter()
        .map(|v| v.trim())
        .find(|v| !v.is_empty())
        .map(str::to_owned);
    let home = home_jwt.is_some();
    // Go main: PGSTORE_*, OBJECTSTORE_* or GITSTORE_* keep config and auth files in a
    // remote store, served from its local mirror (never in Home mode).
    let env = |key: &str| std::env::var(key).ok();
    let store = match cpa_store::select(&env, &wd, home) {
        Some(selection) => match cpa_store::bootstrap(selection, &wd).await {
            Ok(store) => Some(store),
            Err(line) => {
                tracing::error!("{line}");
                return Ok(());
            }
        },
        None => None,
    };
    let config_path = match &store {
        Some(store) => store.config_path.clone(),
        None if args.config.is_empty() => wd.join("config.yaml"),
        None => PathBuf::from(&args.config),
    };
    let cloud = std::env::var("DEPLOY").is_ok_and(|v| v == "cloud");
    // Go main: with a Home JWT the config comes from Home; the config path stays for
    // downstream components but need not exist.
    let (mut config, home_config) = match &home_jwt {
        Some(jwt) => match home::bootstrap(jwt, args.home_disable_cluster_discovery).await {
            Ok((home_config, config)) => (config, Some(home_config)),
            Err(line) => {
                tracing::error!("{line}");
                return Ok(());
            }
        },
        None => (load_config(&config_path, cloud)?, None),
    };
    if let Some(store) = &store {
        config.auth_dir = store.auth_dir.clone();
        tracing::info!("{}", store.enabled);
    }
    // The runtime was sized from -config (or ./config.yaml) before a store or Home
    // supplied the config it now runs on.
    if (store.is_some() || home_config.is_some())
        && std::env::var_os("TOKIO_WORKER_THREADS").is_none()
        && let Some(n) = config
            .document
            .get("worker-threads")
            .and_then(serde_yaml_ng::Value::as_u64)
        && usize::try_from(n).ok() != Some(tokio::runtime::Handle::current().metrics().num_workers())
    {
        let source = match &home_config {
            Some(_) => "the config Home supplies".to_owned(),
            None => config_path.display().to_string(),
        };
        tracing::warn!(
            "worker-threads in {source} is ignored: it is read only from -config/./config.yaml; set TOKIO_WORKER_THREADS"
        );
    }
    let config_present = match &home_config {
        // Go: `configFileExists = cfg.Port != 0` for a config loaded from Home.
        Some(_) => !cloud || config.port != 0,
        None => !cloud || cloud_config_present(&config_path, &config),
    };
    // Go ConfigureLogOutput and SetLogLevel, before any login command.
    cpa_server::logging::configure(&config);
    tracing::info!("{}", banner());
    let before = store.as_ref().map(|store| auth_files(&store.auth_dir));
    // Go: the loaded config reaches the plugins, and a plugin flag hands the run to the
    // plugins that own it.
    if let Some(code) = plugin_cli::execute(&plugins, &config, &config_path, &builtin).await {
        if let (Some(store), Some(before)) = (&store, &before) {
            persist_login(store, before).await;
        }
        if code != 0 {
            std::process::exit(code);
        }
        return Ok(());
    }
    let ran = command(&args, &config).await;
    if !matches!(ran, Ok(false)) {
        if let (Some(store), Some(before)) = (&store, &before) {
            persist_login(store, before).await;
        }
        return ran.map(|_| ());
    }
    if !config_present {
        wait_for_cloud_deploy().await;
        return Ok(());
    }
    if args.local_model && (!args.tui || args.standalone) {
        tracing::info!("Local model mode: using embedded model catalogs, remote model updates disabled");
    }
    if args.tui {
        if args.standalone {
            standalone_tui(
                config,
                config_path,
                args.password,
                args.local_model,
                store,
                home_config,
                plugins,
            )
            .await;
        } else {
            let base = resolve_management_base_url(&args.management_base_url, &config);
            remote_tui(base, args.password).await;
        }
        return Ok(());
    }
    // Only the plain server detaches: discovery, login, import, TUI and plugin
    // commands keep their console.
    if args.log_file.is_some() {
        free_console();
    }
    serve(
        config,
        config_path,
        args.password,
        args.local_model,
        store,
        home_config,
        plugins,
        shutdown_signal(),
        None,
    )
    .await
}

/// A direct Windows Run entry needs no persistent console or shell parent; logs go
/// to the `--log-file` instead.
fn free_console() {
    #[cfg(windows)]
    {
        #[link(name = "kernel32")]
        unsafe extern "system" {
            fn FreeConsole() -> i32;
        }
        // SAFETY: detaches only this process from its console; no pointers.
        unsafe {
            FreeConsole();
        }
    }
}

/// Go's `commandMode`: a login or import flag is set.
fn command_mode(args: &Args) -> bool {
    !args.vertex_import.is_empty()
        || args.antigravity_login
        || args.codex_login
        || args.codex_device_login
        || args.claude_login
        || args.kimi_login
        || args.kimi_ai_login
        || args.xai_login
        || args.devin_login
        || args.meta_login
}

/// Go's TUI client mode: a pure management client; the server runs elsewhere.
async fn remote_tui(base: String, password: String) {
    let run = tokio::task::spawn_blocking(move || tui::run(&base, &password, None, io::stdout())).await;
    if let Err(e) = run.map_err(io::Error::other).and_then(|r| r) {
        eprintln!("TUI error: {e}");
    }
}

/// Go `resolveManagementBaseURL`: the flag, then `remote-management.base-url`, then
/// the local server's port (8317 when unset).
fn resolve_management_base_url(flag: &str, config: &Config) -> String {
    let configured = config
        .document
        .get("management")
        .and_then(|m| m.get("base-url"))
        .and_then(serde_yaml_ng::Value::as_str)
        .unwrap_or_default();
    management_base_url(flag, configured, i64::from(config.port))
}

fn management_base_url(flag: &str, configured: &str, port: i64) -> String {
    for url in [flag, configured] {
        if !url.trim().is_empty() {
            return url.trim().to_owned();
        }
    }
    format!("http://127.0.0.1:{}", if port > 0 { port } else { 8317 })
}

/// Go's standalone TUI points stdout and stderr at /dev/null while it runs and draws on
/// the original stdout; dropping this restores both.
#[cfg(unix)]
struct QuietStdio {
    out: libc::c_int,
    err: libc::c_int,
}

#[cfg(unix)]
impl QuietStdio {
    /// Silences fds 1 and 2; returns the guard and a handle on the original stdout.
    fn start() -> io::Result<(Self, std::fs::File)> {
        use std::os::fd::{AsRawFd, FromRawFd};
        io::Write::flush(&mut io::stdout())?;
        let null = std::fs::OpenOptions::new().write(true).open("/dev/null")?;
        // SAFETY: plain fd duplication; each new fd is owned by the guard or the File.
        unsafe {
            let (out, err, tty) = (libc::dup(1), libc::dup(2), libc::dup(1));
            if out < 0 || err < 0 || tty < 0 {
                return Err(io::Error::last_os_error());
            }
            libc::dup2(null.as_raw_fd(), 1);
            libc::dup2(null.as_raw_fd(), 2);
            Ok((QuietStdio { out, err }, std::fs::File::from_raw_fd(tty)))
        }
    }
}

#[cfg(unix)]
impl Drop for QuietStdio {
    fn drop(&mut self) {
        // SAFETY: restores the fds saved in `start` and closes the saved copies.
        unsafe {
            libc::dup2(self.out, 1);
            libc::dup2(self.err, 2);
            libc::close(self.out);
            libc::close(self.err);
        }
    }
}

/// ponytail: Windows keeps the console handles. Go swaps only its `os.Stdout`
/// variable there too; the log lines already go to the hook, so only a stray direct
/// print could reach the screen.
#[cfg(not(unix))]
struct QuietStdio;

#[cfg(not(unix))]
impl QuietStdio {
    fn start() -> io::Result<(Self, io::Stdout)> {
        Ok((QuietStdio, io::stdout()))
    }
}

/// Go's `-tui -standalone`: the server runs in this process with a local management
/// password (`tui-<pid>-<nanos>` unless -password is set), its log lines feed the logs
/// tab, and it stops after the TUI quits.
async fn standalone_tui(
    config: Config,
    config_path: PathBuf,
    password: String,
    local_model: bool,
    store: Option<cpa_store::Store>,
    home_config: Option<cpa_home::HomeConfig>,
    plugins: cpa_plugin::Host,
) {
    let hook = cpa_server::logging::capture(2000);
    let (quiet, out): (Option<QuietStdio>, Box<dyn io::Write + Send>) = match QuietStdio::start() {
        Ok((guard, tty)) => (Some(guard), Box::new(tty)),
        Err(_) => (None, Box::new(io::stdout())),
    };
    let password = if password.is_empty() {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos());
        format!("tui-{}-{nanos}", std::process::id())
    } else {
        password
    };
    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    let shutdown = async {
        let _ = stopped.await;
    };
    let (listening, listener) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(serve(
        config,
        config_path,
        password.clone(),
        local_model,
        store,
        home_config,
        plugins,
        shutdown,
        Some(listening),
    ));
    // No listener means serve failed; its error is logged below.
    let base = match listener.await {
        Ok(listener) => standalone_base_url(listener),
        Err(_) => Err("embedded server is not ready".to_owned()),
    };
    let result = match &base {
        Ok(base) if tui::wait_ready(base, &password).await => {
            let base = base.clone();
            let run = tokio::task::spawn_blocking(move || tui::run(&base, &password, Some(hook), out)).await;
            run.map_err(|e| e.to_string())
                .and_then(|r| r.map_err(|e| e.to_string()))
        }
        Ok(_) => Err("embedded server is not ready".to_owned()),
        Err(e) => Err(e.clone()),
    };
    // Restores stdout and stderr on Unix; outside Unix nothing was redirected.
    #[cfg_attr(not(unix), allow(clippy::drop_non_drop))]
    drop(quiet);
    cpa_server::logging::release();
    if let Err(e) = result {
        eprintln!("TUI error: {e}");
    }
    let _ = stop.send(());
    if let Ok(Err(e)) = server.await {
        tracing::error!("{e:#}");
    }
}

/// The top-level `*.json` files of the auth directory.
fn auth_files(dir: &Path) -> std::collections::BTreeMap<PathBuf, Vec<u8>> {
    std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().to_lowercase().ends_with(".json"))
        .filter_map(|e| Some((e.path(), std::fs::read(e.path()).ok()?)))
        .collect()
}

/// Go's token store `Save` after a login command: what the login wrote goes to the
/// remote store, or the next bootstrap would not know it.
async fn persist_login(store: &cpa_store::Store, before: &std::collections::BTreeMap<PathBuf, Vec<u8>>) {
    for (path, data) in auth_files(&store.auth_dir) {
        if before.get(&path) == Some(&data) {
            continue;
        }
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        if let Err(error) = store
            .persister
            .persist_auth_files(format!("Update auth {name}"), vec![path])
            .await
        {
            tracing::error!("failed to save auth {name} to the token store: {error:#}");
        }
    }
}

/// Go's command modes, in Go's order; `Ok(false)` when no command flag is set.
async fn command(args: &Args, config: &Config) -> anyhow::Result<bool> {
    // Go DoVertexImport: failures are logged and the command still exits normally.
    if !args.vertex_import.is_empty() {
        match cpa_exec::vertex_auth::import(&config.auth_dir, &args.vertex_import, &args.vertex_import_prefix) {
            Ok(path) => println!("Vertex credentials imported: {}", path.display()),
            Err(error) => tracing::error!("{error}"),
        }
        return Ok(true);
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
        return Ok(true);
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
        return Ok(true);
    }
    if args.kimi_login || args.kimi_ai_login {
        let provider = if args.kimi_login { "kimi" } else { "kimi-ai" };
        cpa_exec::kimi_auth::login(provider, config, args.no_browser).await?;
        return Ok(true);
    }
    if args.xai_login {
        // Go DoXAILogin: a failure is logged and the command still exits normally.
        if let Err(error) = cpa_exec::xai_auth::login(config, args.no_browser).await {
            tracing::error!("xAI authentication failed: {}", String::from_utf8_lossy(&error.body));
        }
        return Ok(true);
    }
    if args.devin_login {
        // Go DoDevinLogin: a failure is logged and the command still exits normally.
        cpa_exec::devin_auth::login(config, args.no_browser, args.oauth_callback_port).await;
        return Ok(true);
    }
    if args.meta_login {
        cpa_exec::meta_auth::login(config, args.no_browser).await?;
        return Ok(true);
    }
    Ok(false)
}

/// The standalone TUI's server address: the bound one, or loopback for a wildcard bind.
/// The local password works only from loopback, so other hosts are refused, and so is
/// TLS, which would need the server certificate to be trusted for that address.
fn standalone_base_url((addr, tls): (SocketAddr, bool)) -> Result<String, String> {
    if tls {
        return Err("the standalone TUI does not support server.tls; turn it off or run the TUI as a client".into());
    }
    let ip = match addr.ip() {
        std::net::IpAddr::V4(ip) if ip.is_unspecified() => Ipv4Addr::LOCALHOST.into(),
        std::net::IpAddr::V6(ip) if ip.is_unspecified() => Ipv6Addr::LOCALHOST.into(),
        ip if ip.is_loopback() => ip,
        ip => {
            return Err(format!(
                "the standalone TUI needs a loopback or wildcard host, not {ip}; run the TUI as a client"
            ));
        }
    };
    Ok(format!("http://{}", SocketAddr::new(ip, addr.port())))
}

/// Go `Service.ensureAuthDir`.
fn ensure_auth_dir(dir: &Path) -> anyhow::Result<()> {
    match std::fs::metadata(dir) {
        Ok(meta) if meta.is_dir() => Ok(()),
        Ok(_) => anyhow::bail!("cliproxy: auth path exists but is not a directory: {}", dir.display()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            let mut builder = std::fs::DirBuilder::new();
            builder.recursive(true);
            #[cfg(unix)]
            std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o755);
            builder
                .create(dir)
                .with_context(|| format!("cliproxy: failed to create auth directory {}", dir.display()))?;
            tracing::info!("created missing auth directory: {}", dir.display());
            Ok(())
        }
        Err(e) => Err(e).with_context(|| format!("cliproxy: error checking auth directory {}", dir.display())),
    }
}

/// `listening` (standalone TUI only) receives the bound address and whether TLS is on.
#[allow(clippy::too_many_arguments)]
async fn serve(
    config: Config,
    config_path: PathBuf,
    password: String,
    local_model: bool,
    store: Option<cpa_store::Store>,
    home_config: Option<cpa_home::HomeConfig>,
    plugins: cpa_plugin::Host,
    shutdown: impl std::future::Future<Output = ()>,
    listening: Option<tokio::sync::oneshot::Sender<(SocketAddr, bool)>>,
) -> anyhow::Result<()> {
    if config.api_keys.is_empty() && home_config.is_none() {
        tracing::warn!("access.api-keys is empty: the proxy API is open to anyone who can reach it");
    }
    // Go `ensureAuthDir`: outside Home mode a missing auth-dir is created, so the
    // watcher has a folder to watch for the first login.
    if home_config.is_none() {
        ensure_auth_dir(&config.auth_dir)?;
    }
    // Auth-dir files and config API keys, synthesized as Go's watcher does. Home mode
    // runs on credentials Home dispatches per request.
    let credentials = match home_config {
        Some(_) => Vec::new(),
        None => cpa_core::config::credentials::load(&config),
    };
    tracing::info!(credentials = credentials.len(), "credentials loaded");
    let listener =
        bind(&config.host, config.port).with_context(|| format!("binding {}:{}", config.host, config.port))?;
    let listener = tokio::net::TcpListener::from_std(listener)?;
    // Go `Server.Start`: TLS is validated after the listener is open.
    let tls = cpa_server::listener::tls_acceptor(&config)?;
    tracing::info!(addr = %listener.local_addr()?, tls = tls.is_some(), "listening");
    let standalone = listening.is_some();
    if let Some(listening) = listening {
        let _ = listening.send((listener.local_addr()?, tls.is_some()));
    }
    let executors = Executors {
        claude: ClaudeExecutor::new(DEFAULT_BASE_URL)?,
        codex: cpa_exec::codex::CodexExecutor::new()?,
        devices: Default::default(),
        openai: Default::default(),
        google: Default::default(),
    };
    let rt = Arc::new(Runtime::new(config, credentials, executors).with_plugin_host(plugins));
    rt.set_local_model(local_model);
    if let Some(cooldown) = store.as_ref().and_then(|store| store.cooldown.clone()) {
        rt.set_cooldown_backend(cooldown);
    }
    // Go `startModelCatalogUpdaters`.
    cpa_server::model_updater::start(&rt, home_config.is_some());
    let home = home_config.map(|home_config| {
        let dispatcher = Arc::new(home::Dispatcher::new(&rt));
        rt.set_remote_dispatch(Some(dispatcher.clone()));
        let shutdown = tokio_util::sync::CancellationToken::new();
        let task = home::spawn_subscriber(home_config, rt.clone(), dispatcher, shutdown.clone());
        (shutdown, task)
    });
    // Translators and thinking validation read model capabilities through the global
    // overlay; without it they see only the static catalog.
    cpa_server::install_registry(&rt);
    rt.start_auto_refresh();
    let options = cpa_server::management::Options {
        local_password: password,
        standalone,
        store: store.map(|store| store.persister),
        ..Default::default()
    };
    let management = cpa_server::management::Management::with_options(rt.clone(), config_path, options);
    if home.is_some() {
        // Go enables the usage queue in Home mode whatever the management secret
        // (`redisqueue.SetEnabled(... || cfg.Home.Enabled)`); the forwarder drains it.
        rt.usage_queue().configure(true, &rt.config());
    }
    // Go applies the plugin config before serving; later publishes resync the host.
    cpa_server::plugins::start(&rt).await;
    // Home mode: the config comes from Home, and management answers 404 (Go
    // `managementAvailable`).
    let _watcher = home.is_none().then(|| cpa_server::watching::start(&management));
    let advertiser = {
        let rt = rt.clone();
        // The served transport decides `tls=`: HTTPS exactly when server.tls loaded.
        let published = rt.subscribe_config();
        discovery::advertise::Advertiser::spawn(move || rt.config(), published, tls.is_some())
    };
    // Go applies its CORS middleware to every route, not only management.
    let app = match &home {
        Some(_) => cpa_server::router(rt.clone()).layer(axum::middleware::from_fn_with_state(
            rt.clone(),
            cpa_server::remote::gate,
        )),
        None => cpa_server::app(rt.clone(), cpa_server::management::router(management.clone())),
    };
    let app = if home.is_none() {
        cpa_server::safe_mode::router(&rt, app)
    } else {
        app
    };
    let app = app.layer(axum::middleware::from_fn(cpa_server::management::cors));
    let app = cpa_server::request_logging::router(&management, app);
    let app = cpa_server::observability::router(&rt, app);
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    let app = heap_trim::layer(app);
    // Go's listener also serves the Redis protocol (usage queue) to management clients;
    // in Home mode it answers "ERR redis usage output disabled in home mode".
    let mut server = Box::pin(cpa_server::listener::serve_with_resp(
        listener,
        app,
        tls,
        Some(management),
    ));
    let mut home = home;
    // Go `cancelServiceRun`: a Home subscriber that stops on its own (an unsafe drain or
    // an unsettled dispatch) stops the service; executions it could not drain end with
    // the process instead of running on unaccounted.
    let home_stopped = async {
        match home.as_mut() {
            Some((_, task)) => {
                let _ = task.await;
            }
            None => std::future::pending::<()>().await,
        }
    };
    let mut home_failed = false;
    let served = tokio::select! {
        r = &mut server => r.map_err(anyhow::Error::from),
        _ = shutdown => Ok(()),
        _ = home_stopped => {
            home_failed = true;
            Err(anyhow::anyhow!("home subscriber stopped; shutting down"))
        }
    };
    // Go's Service.Shutdown sends the mDNS goodbye (shutdownDiscovery) before
    // Server.Stop, which closes the HTTP server without draining (`http.Server.Close`);
    // dropping the server future afterwards does the same.
    advertiser.shutdown().await;
    drop(server);
    // Go drains the Home registry and flushes releases before exiting.
    if let Some((shutdown, task)) = home
        && !home_failed
    {
        shutdown.cancel();
        let _ = task.await;
    }
    served
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `worker-threads` from the config file; anything but a positive number, or no
    /// file, falls back to min(CPUs, 2). (`TOKIO_WORKER_THREADS` is not set in tests.)
    #[test]
    fn worker_threads_come_from_the_config_file() {
        let dir = std::env::temp_dir().join(format!("cliproxy-workers-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.yaml");
        let default = std::thread::available_parallelism().map_or(1, |n| n.get().min(2));
        std::fs::write(&path, "port: 1\nworker-threads: 5\n").unwrap();
        assert_eq!(worker_threads(Some(&path)), 5);
        for text in [
            "worker-threads: 0\n",
            "worker-threads: -3\n",
            "worker-threads: many\n",
            ": not yaml [",
        ] {
            std::fs::write(&path, text).unwrap();
            assert_eq!(worker_threads(Some(&path)), default, "{text:?}");
        }
        assert_eq!(worker_threads(Some(&dir.join("missing.yaml"))), default);
        assert_eq!(worker_threads(None), default);
        std::fs::remove_dir_all(dir).unwrap();
    }

    // Expected values: Go resolveManagementBaseURL (tests/fixtures/discovery_main_go.json).
    #[test]
    fn management_base_url_follows_go() {
        let fixture: serde_json::Value =
            serde_json::from_str(include_str!("../tests/fixtures/discovery_main_go.json")).unwrap();
        let cases = fixture["management_base_url"].as_array().unwrap();
        assert!(cases.len() >= 6);
        for case in cases {
            let got = management_base_url(
                case["flag"].as_str().unwrap(),
                case["remote"].as_str().unwrap(),
                case["port"].as_i64().unwrap(),
            );
            assert_eq!(got, case["out"].as_str().unwrap(), "{case}");
        }
        let config = Config::parse("config-version: 8\nmanagement:\n  base-url: ' http://y '\n").unwrap();
        assert_eq!(resolve_management_base_url("", &config), "http://y");
    }

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

    /// A flag the binary does not define may be a boolean plugin flag, so a discover
    /// flag after it still selects discover mode, before any plugin loads. Built-in
    /// flags that take a value keep consuming it as Go does.
    #[test]
    fn a_plugin_flag_before_discover_keeps_discover_mode() {
        let argv = |a: &[&str]| a.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>();
        assert!(argv_enables_bool_flag(
            &argv(&["-plugin-verbose", "-discover"]),
            "discover"
        ));
        assert!(argv_enables_bool_flag(
            &argv(&["--plugin-verbose", "--discover-json"]),
            "discover-json"
        ));
        assert!(argv_enables_bool_flag(
            &argv(&["-plugin-verbose", "-discover=true"]),
            "discover"
        ));
        // A plugin flag still takes an ordinary value.
        assert!(argv_enables_bool_flag(
            &argv(&["-plugin-mode", "fast", "-discover"]),
            "discover"
        ));
        assert!(!argv_enables_bool_flag(
            &argv(&["-plugin-mode", "x", "fast", "-discover"]),
            "discover"
        ));
        // A built-in value flag consumes the next token, as in Go.
        assert!(!argv_enables_bool_flag(&argv(&["-config", "-discover"]), "discover"));
        assert!(!argv_enables_bool_flag(
            &argv(&["-password", "--discover-json"]),
            "discover-json"
        ));
    }

    /// `-plugin-verbose -discover` selects discover mode, and the parse that follows
    /// (without plugins) accepts that flag; another undefined flag still fails.
    #[test]
    fn a_plugin_flag_before_discover_parses_in_discover_mode() {
        let parse = |a: &[&str]| {
            let raw: Vec<String> = std::iter::once("cliproxy")
                .chain(a.iter().copied())
                .map(str::to_owned)
                .collect();
            assert!(argv_enables_bool_flag(&raw[1..], "discover"), "{a:?}");
            let flags = go_flags(&Args::command(), raw.clone(), &PrescanBools::from_args(&raw[1..]))?;
            Args::try_parse_from(flags).map_err(|e| e.to_string())
        };
        assert!(parse(&["-plugin-verbose", "-discover"]).unwrap().discover);
        assert!(
            parse(&["--plugin-verbose", "--discover", "-discover-timeout", "5"])
                .unwrap()
                .discover
        );
        assert!(parse(&["-plugin-mode", "fast", "-discover"]).is_err());
    }

    #[test]
    fn every_go_flag_parses_in_single_and_double_dash_form() {
        let argv = |a: &[&str]| {
            go_flags(
                &Args::command(),
                std::iter::once("cliproxy").chain(a.iter().copied()).map(String::from),
                &(),
            )
            .unwrap()
        };
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
        let sub = |a: &[&str]| {
            DiscoverArgs::try_parse_from(
                go_flags(
                    &DiscoverArgs::command(),
                    std::iter::once("discover").chain(a.iter().copied()).map(String::from),
                    &(),
                )
                .unwrap(),
            )
        };
        let parsed = sub(&[
            "-timeout", "5", "-json", "-include", "a,b", "-include", "a", "-config", "x.yaml",
        ])
        .unwrap();
        assert_eq!(
            (parsed.timeout, parsed.json, csv_flags(&parsed.include)),
            (5, true, vec!["a".into(), "b".into(), "a".into()])
        );
        // Go flag rules that plain clap rejects.
        let parsed = sub(&[
            "--json=true",
            "-timeout",
            "2",
            "-timeout",
            "-10",
            "-config",
            "-local.yaml",
        ])
        .unwrap();
        assert_eq!(
            (parsed.json, parsed.timeout, parsed.config.as_str()),
            (true, -10, "-local.yaml")
        );
        assert!(!sub(&["--json=false"]).unwrap().json);
        assert!(!sub(&["-json", "-json=0"]).unwrap().json);
        assert_eq!(sub(&["-timeout=0x10"]).unwrap().timeout, 16);
        // Parsing stops at the first operand; Go ignores the rest.
        let parsed = sub(&["-json", "extra", "-timeout", "9"]).unwrap();
        assert_eq!((parsed.json, parsed.timeout), (true, 3));
        assert!(sub(&["-json=maybe"]).is_err());
        assert!(sub(&["-nope"]).is_err());
        assert!(sub(&["-timeout"]).is_err());
        assert!(sub(&["-timeout", "1.5"]).is_err());
        assert!(sub(&["---x"]).is_err());
    }

    #[test]
    fn prescan_reads_every_spelling() {
        // The same table for each prescanned flag, written for -working-dir.
        for flag in ["working-dir", "log-file"] {
            let scan = |a: &[&str]| {
                let args: Vec<String> = a.iter().map(|s| s.replace("working-dir", flag)).collect();
                prescan_value(&args, flag)
            };
            for spelling in [
                &["-working-dir", "d"][..],
                &["--working-dir", "d"],
                &["-working-dir=d"],
                &["--working-dir=d"],
                &["-tui", "-config", "c.yaml", "--working-dir", "d", "-local-model"],
                &["-working-dir", "x", "-working-dir=d"],
                // An undefined (plugin) flag with a value, or a boolean one before a flag.
                &["-plugin-mode", "fast", "-working-dir", "d"],
                &["-plugin-verbose", "-working-dir", "d"],
            ] {
                assert_eq!(scan(spelling).as_deref(), Some("d"), "{flag}: {spelling:?}");
            }
            // The value may start with a dash, as for every Go value flag.
            assert_eq!(scan(&["-working-dir", "-d"]).as_deref(), Some("-d"), "{flag}");
            assert_eq!(scan(&["-working-dir="]).as_deref(), Some(""), "{flag}");
            // Values of other flags, operands, `--` and a missing value are not it.
            assert_eq!(scan(&["-config", "-working-dir", "x"]), None, "{flag}");
            assert_eq!(scan(&["-password", "--working-dir=x"]), None, "{flag}");
            assert_eq!(scan(&["run", "-working-dir", "x"]), None, "{flag}");
            assert_eq!(scan(&["--", "-working-dir", "x"]), None, "{flag}");
            assert_eq!(scan(&["-working-dir"]), None, "{flag}");
            assert_eq!(scan(&["discover", "-working-dir", "x"]), None, "{flag}");
        }
        let args = ["-working-dir", "-log-file", "l"].map(str::to_owned);
        assert_eq!(prescan_value(&args, "log-file"), None);
        // The full parse agrees, and neither flag is passed to plugins.
        let raw = ["cliproxy", "-tui", "--working-dir=d", "-log-file", "l"].map(str::to_owned);
        let cmd = Args::command();
        let matches = cmd.clone().get_matches_from(go_flags(&cmd, raw.clone(), &()).unwrap());
        let args = Args::from_arg_matches(&matches).unwrap();
        assert_eq!(args.working_dir.as_deref(), Some(Path::new("d")));
        assert_eq!(args.log_file.as_deref(), Some(Path::new("l")));
        assert_eq!(prescan_value(&raw[1..], "working-dir").as_deref(), Some("d"));
        assert_eq!(prescan_value(&raw[1..], "log-file").as_deref(), Some("l"));
        let builtin = plugin_cli::builtin_values(&cmd, &matches);
        assert!(builtin.iter().all(|(n, _)| n != "working-dir" && n != "log-file"));
        assert!(builtin.iter().any(|(n, v)| n == "tui" && v == "true"));
    }

    #[test]
    fn go_int_matches_strconv_base_zero() {
        for (s, want) in [
            ("3", Some(3)),
            ("-10", Some(-10)),
            ("+7", Some(7)),
            ("0x1f", Some(31)),
            ("010", Some(8)),
        ] {
            assert_eq!(go_int(s).ok(), want, "{s}");
        }
        for (s, want) in [
            ("0b101", Some(5)),
            ("0o17", Some(15)),
            ("1_000", Some(1000)),
            ("0_10", Some(8)),
            ("", None),
        ] {
            assert_eq!(go_int(s).ok(), want, "{s}");
        }
        for bad in ["9223372036854775808", "_1", "1_", "1__0", "0x", "1.5", "0x_"] {
            assert!(go_int(bad).is_err(), "{bad}");
        }
        assert_eq!(go_int("0x_1f").ok(), Some(31));
        assert_eq!(go_int("-9223372036854775808").ok(), Some(i64::MIN));
    }

    #[test]
    fn standalone_url_follows_the_listener() {
        let url = |addr: &str, tls| standalone_base_url((addr.parse().unwrap(), tls));
        assert_eq!(url("127.0.0.1:4100", false).unwrap(), "http://127.0.0.1:4100");
        assert_eq!(url("0.0.0.0:4101", false).unwrap(), "http://127.0.0.1:4101");
        assert_eq!(url("[::]:4102", false).unwrap(), "http://[::1]:4102");
        assert_eq!(url("[::1]:4103", false).unwrap(), "http://[::1]:4103");
        assert!(url("192.0.2.7:4104", false).unwrap_err().contains("192.0.2.7"));
        assert!(url("127.0.0.1:4105", true).unwrap_err().contains("server.tls"));
    }

    /// `-tui -standalone` with no management key anywhere and `port: 0`: the TUI finds
    /// the ephemeral port and signs in with the generated password.
    #[tokio::test]
    async fn standalone_server_starts_without_any_secret() {
        let dir = std::env::temp_dir().join(format!("cpa-standalone-{}", std::process::id()));
        let auth = dir.join("auth");
        std::fs::create_dir_all(&auth).unwrap();
        let path = dir.join("config.yaml");
        std::fs::write(
            &path,
            format!("host: 127.0.0.1\nport: 0\nauth-dir: '{}'\n", auth.display()),
        )
        .unwrap();
        let config = Config::load(&path).unwrap();
        assert!(config.management.secret_key.is_empty());
        let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
        let (listening, listener) = tokio::sync::oneshot::channel();
        let server = tokio::spawn(serve(
            config,
            path,
            "fake-tui-password".into(),
            true,
            None,
            None,
            cpa_plugin::Host::default(),
            async {
                let _ = stopped.await;
            },
            Some(listening),
        ));
        let base = standalone_base_url(listener.await.unwrap()).unwrap();
        assert_ne!(base, "http://127.0.0.1:0");
        assert!(tui::wait_ready(&base, "fake-tui-password").await);
        let _ = stop.send(());
        server.await.unwrap().unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn cloud_mode_treats_missing_empty_and_broken_files_as_empty_config() {
        let dir = std::env::temp_dir().join(format!("cpa-cloud-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let missing = dir.join("missing.yaml");
        assert_eq!(load_config(&missing, true).unwrap().port, 0);
        assert!(load_config(&missing, false).is_err());
        assert_eq!(load_config(&dir, true).unwrap().port, 0, "a directory is standby too");
        for text in ["port: [\n", "port: wrong\n", "- a\n- b\n"] {
            let broken = dir.join("broken.yaml");
            std::fs::write(&broken, text).unwrap();
            assert_eq!(load_config(&broken, true).unwrap().port, 0, "{text}");
            assert!(load_config(&broken, false).is_err(), "{text}");
        }
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
            err.kind(),
            io::ErrorKind::AddrInUse,
            "must not silently fall back to IPv4"
        );
    }

    #[cfg(unix)]
    #[test]
    fn only_missing_ipv6_falls_back_to_ipv4() {
        assert!(no_ipv6(&io::Error::from_raw_os_error(libc::EAFNOSUPPORT)));
        assert!(no_ipv6(&io::Error::from_raw_os_error(libc::EADDRNOTAVAIL)));
        assert!(!no_ipv6(&io::Error::from_raw_os_error(libc::EADDRINUSE)));
        assert!(!no_ipv6(&io::Error::from_raw_os_error(libc::EACCES)));
    }
}
