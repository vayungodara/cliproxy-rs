//! The `discover` command (Go `internal/cmd/discover.go`): scan options, interface
//! filters from config, and the exact text and JSON Go prints.
use std::io::Write;
use std::net::IpAddr;
use std::path::Path;
use std::time::Duration;

use serde_json::{Value, json};
use serde_yaml_ng::Value as Yaml;

use super::{DEFAULT_SERVICE_TYPE, DiscoveredService, go_trim};

#[derive(Clone, Debug, Default)]
pub struct Options {
    pub timeout: Duration,
    pub json: bool,
    pub service_type: String,
    pub include: Vec<String>,
    pub exclude: Vec<String>,
}

/// Go `ParseInterfaceList`: comma-split, trimmed, blanks and repeats dropped.
pub fn parse_interface_list<S: AsRef<str>>(values: &[S]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for raw in values {
        for part in raw.as_ref().split(',') {
            let part = go_trim(part);
            if !part.is_empty() && !out.iter().any(|o| o == part) {
                out.push(part.to_owned());
            }
        }
    }
    out
}

/// Go `ResolveDiscoveryInterfaceFilters`: any CLI filter replaces both config filters.
pub fn resolve_filters(cli: (Vec<String>, Vec<String>), cfg: (Vec<String>, Vec<String>)) -> (Vec<String>, Vec<String>) {
    if !cli.0.is_empty() || !cli.1.is_empty() {
        cli
    } else {
        cfg
    }
}

/// Go `LoadDiscoveryScanFilters`: `discovery.interfaces` from the legacy top-level key
/// only (Go's scan does not read `server.discovery`). Missing or undecodable files
/// give no filters.
pub fn load_scan_filters(path: &str) -> (Vec<String>, Vec<String>) {
    let path = match go_trim(path) {
        "" => std::env::current_dir()
            .map(|d| d.join("config.yaml"))
            .unwrap_or_else(|_| "config.yaml".into()),
        p => Path::new(p).to_path_buf(),
    };
    let Ok(text) = std::fs::read_to_string(path) else {
        return Default::default();
    };
    let Ok(doc) = serde_yaml_ng::from_str::<Yaml>(&text) else {
        return Default::default();
    };
    // yaml.v3 decodes null into zero values and fails on a wrong-kind node anywhere
    // on the path, which drops both lists.
    let mapping = |v: Option<&Yaml>| -> Option<Option<Yaml>> {
        match v {
            None | Some(Yaml::Null) => Some(None),
            Some(m @ Yaml::Mapping(_)) => Some(Some(m.clone())),
            Some(_) => None,
        }
    };
    let Some(root) = mapping(Some(&doc)) else {
        return Default::default();
    };
    let Some(discovery) = mapping(root.as_ref().and_then(|r| r.get("discovery"))) else {
        return Default::default();
    };
    let Some(interfaces) = mapping(discovery.as_ref().and_then(|d| d.get("interfaces"))) else {
        return Default::default();
    };
    let list = |k: &str| super::string_list(interfaces.as_ref().and_then(|i| i.get(k)));
    match (list("include"), list("exclude")) {
        (Some(include), Some(exclude)) => (parse_interface_list(&include), parse_interface_list(&exclude)),
        _ => Default::default(),
    }
}

/// Go `time.Duration.String` for the non-negative durations the scan prints.
pub fn go_duration(d: Duration) -> String {
    let nanos = d.as_nanos();
    if nanos == 0 {
        return "0s".into();
    }
    if nanos < 1_000_000_000 {
        let (unit, div) = match nanos {
            n if n < 1_000 => return format!("{n}ns"),
            n if n < 1_000_000 => ("µs", 1_000u128),
            _ => ("ms", 1_000_000u128),
        };
        return format!("{}{unit}", fraction(nanos, div));
    }
    let secs = nanos / 1_000_000_000;
    let (h, m) = (secs / 3600, secs / 60 % 60);
    let s = fraction(nanos % 60_000_000_000, 1_000_000_000);
    match (h, m) {
        (0, 0) => format!("{s}s"),
        (0, m) => format!("{m}m{s}s"),
        (h, m) => format!("{h}h{m}m{s}s"),
    }
}

/// `n / div` with Go's trimmed decimal fraction.
fn fraction(n: u128, div: u128) -> String {
    let whole = n / div;
    let rest = n % div;
    if rest == 0 {
        return whole.to_string();
    }
    let digits = div.to_string().len() - 1;
    let frac = format!("{rest:0digits$}");
    format!("{whole}.{}", frac.trim_end_matches('0'))
}

/// Go `json.MarshalIndent(v, "", "  ")`, including its HTML-safe escapes.
pub fn go_indent(v: &Value) -> String {
    let text = serde_json::to_string_pretty(v).expect("JSON values serialize");
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '<' => out.push_str("\\u003c"),
            '>' => out.push_str("\\u003e"),
            '&' => out.push_str("\\u0026"),
            '\u{2028}' => out.push_str("\\u2028"),
            '\u{2029}' => out.push_str("\\u2029"),
            c => out.push(c),
        }
    }
    out
}

/// Go `sanitizeTerminal`: printable characters only, tabs become spaces; bidi
/// controls, zero-width and invisible format characters are dropped.
pub fn sanitize_terminal(s: &str) -> String {
    let bidi =
        |c: char| matches!(c, '\u{061C}' | '\u{200E}' | '\u{200F}' | '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}');
    s.chars()
        .filter_map(|c| match c {
            '\t' => Some(' '),
            c if cpa_common::gostr::is_print(c)
                && !bidi(c)
                && !matches!(
                    c,
                    '\u{2028}' | '\u{2029}' | '\u{200B}'..='\u{200F}' | '\u{FEFF}' | '\u{00AD}'
                ) =>
            {
                Some(c)
            }
            _ => None,
        })
        .collect()
}

/// Go `sanitizeDisplayHost`: a plain DNS host name or empty.
pub fn sanitize_display_host(host: &str) -> String {
    let host = sanitize_terminal(go_trim(host.strip_suffix('.').unwrap_or(host)));
    let valid = !host.is_empty()
        && !host.eq_ignore_ascii_case("localhost")
        && host
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'.')
        && !host.contains("..")
        && !host.starts_with(['-', '.'])
        && !host.ends_with(['-', '.']);
    if valid { host } else { String::new() }
}

fn link_local(ip: &IpAddr) -> bool {
    match ip {
        IpAddr::V4(a) => a.is_link_local(),
        IpAddr::V6(a) => a.is_unicast_link_local(),
    }
}

/// Go `preferredDisplayAddresses`: IPv4 first, then routable IPv6, then the host
/// name, then link-local IPv6, then loopback as a last resort.
pub fn preferred_display_addresses(gw: &DiscoveredService) -> (String, Vec<String>) {
    let ok = |ip: &&IpAddr| !ip.is_loopback() && !ip.is_unspecified();
    let all: Vec<String> = gw.ipv4.iter().filter(ok).map(ToString::to_string).collect();
    if let Some(first) = all.first() {
        return (first.clone(), all);
    }
    let (link, routable): (Vec<&IpAddr>, Vec<&IpAddr>) = gw.ipv6.iter().filter(ok).partition(|ip| link_local(ip));
    let link: Vec<String> = link.iter().map(ToString::to_string).collect();
    if let Some(first) = routable.first() {
        let mut all: Vec<String> = routable.iter().map(ToString::to_string).collect();
        all.extend(link);
        return (first.to_string(), all);
    }
    let host = sanitize_display_host(&gw.host);
    if !host.is_empty() {
        let mut all = vec![host.clone()];
        all.extend(link);
        return (host, all);
    }
    match link.first() {
        Some(first) => (first.clone(), link.clone()),
        None => ("127.0.0.1".into(), Vec::new()),
    }
}

/// Go `net.JoinHostPort`.
fn join_host_port(host: &str, port: u16) -> String {
    if host.contains(':') {
        format!("[{host}]:{port}")
    } else {
        format!("{host}:{port}")
    }
}

fn safe_join(items: &[String]) -> String {
    items
        .iter()
        .map(|s| sanitize_terminal(s))
        .collect::<Vec<_>>()
        .join(", ")
}

/// Go `runDiscoverWithOptions`: prints the scan banner, runs `browse(service_type,
/// timeout)` and reports; returns the exit code.
pub async fn run<F, Fut>(opts: &Options, out: &mut impl Write, err: &mut impl Write, browse: F) -> i32
where
    F: FnOnce(String, Duration) -> Fut,
    Fut: std::future::Future<Output = Result<Vec<DiscoveredService>, String>>,
{
    let timeout = if opts.timeout.is_zero() {
        Duration::from_secs(3)
    } else {
        opts.timeout.min(Duration::from_secs(60))
    };
    let service_type = match go_trim(&opts.service_type) {
        "" => DEFAULT_SERVICE_TYPE.to_owned(),
        st => st.to_owned(),
    };
    if !opts.json {
        let _ = writeln!(
            out,
            "Scanning LAN for AI Gateways ({service_type})... (timeout {})",
            go_duration(timeout)
        );
    }
    let gateways = match tokio::time::timeout(timeout + Duration::from_secs(1), browse(service_type, timeout)).await {
        Ok(Ok(g)) => g,
        Ok(Err(e)) => return fail(opts, out, err, &e),
        Err(_) => {
            return fail(
                opts,
                out,
                err,
                "discovery: browse query failed: context deadline exceeded",
            );
        }
    };
    if opts.json {
        let list: Vec<Value> = gateways.iter().map(DiscoveredService::to_json).collect();
        let _ = writeln!(out, "{}", go_indent(&Value::from(list)));
        return 0;
    }
    if gateways.is_empty() {
        let _ = write!(
            out,
            "\nNo AI gateways found on local network.\nTips:\n  1. Ensure the target CPA instance has 'discovery.enabled: true' in its config.yaml.\n  2. Ensure your device is on the same local Wi-Fi / Ethernet subnet (mDNS does not traverse WAN).\n  3. Check that your local firewall allows UDP port 5353 multicast traffic.\n"
        );
        return 0;
    }
    let _ = writeln!(out, "\nFound {} AI Gateway(s) on local network:\n", gateways.len());
    for (i, gw) in gateways.iter().enumerate() {
        let or = |s: String, d: &str| if s.is_empty() { d.to_owned() } else { s };
        let _ = writeln!(
            out,
            "[{}] {} (Product: {}, Version: {})",
            i + 1,
            sanitize_terminal(&gw.instance_name),
            or(sanitize_terminal(&gw.product), "generic"),
            or(sanitize_terminal(&gw.version), "unknown")
        );
        let (primary, all) = preferred_display_addresses(gw);
        let host_port = join_host_port(&primary, gw.port);
        let _ = writeln!(out, "    Host:      {} ({host_port})", sanitize_terminal(&gw.host));
        if all.len() > 1 {
            let _ = writeln!(out, "    Addresses: {}", all.join(", "));
        }
        if !gw.protocols.is_empty() {
            let _ = writeln!(out, "    Protocols: {}", safe_join(&gw.protocols));
        }
        if !gw.features.is_empty() {
            let _ = writeln!(out, "    Features:  {}", safe_join(&gw.features));
        }
        let auth = match (gw.auth_required, gw.auth_methods.is_empty()) {
            (false, _) => "No".to_owned(),
            (true, true) => "Required".to_owned(),
            (true, false) => format!("Required ({})", safe_join(&gw.auth_methods)),
        };
        let _ = writeln!(out, "    Auth:      {auth}");
        let scheme = if gw.raw_txt.get("tls").is_some_and(|v| v == "1") {
            "https"
        } else {
            "http"
        };
        let _ = writeln!(out, "    Base URLs:");
        let openai = gw
            .endpoints
            .get("openai")
            .map_or("/v1".into(), |p| sanitize_terminal(p));
        let _ = writeln!(out, "      - OpenAI:    {scheme}://{host_port}{openai}");
        for (key, label) in [("anthropic", "Anthropic"), ("gemini", "Gemini")] {
            if let Some(path) = gw.endpoints.get(key) {
                let _ = writeln!(
                    out,
                    "      - {label}: {}{scheme}://{host_port}{}",
                    " ".repeat(9 - label.len()),
                    sanitize_terminal(path)
                );
            }
        }
        let _ = writeln!(out);
    }
    0
}

fn fail(opts: &Options, out: &mut impl Write, err: &mut impl Write, e: &str) -> i32 {
    if opts.json {
        let _ = writeln!(out, "{}", go_indent(&json!({"error": e, "gateways": []})));
    } else {
        let _ = writeln!(err, "Error scanning LAN: {e}");
    }
    1
}

/// Go `DoDiscoverWithOptions` on the real LAN.
pub async fn discover(opts: &Options) -> i32 {
    let (include, exclude) = (opts.include.clone(), opts.exclude.clone());
    let mut out = std::io::stdout();
    let mut err = std::io::stderr();
    run(opts, &mut out, &mut err, |service_type, timeout| async move {
        let ifaces = super::iface::filter(&include, &exclude)?;
        if ifaces.is_empty() {
            return Err("no qualified physical interfaces found for LAN discovery".into());
        }
        super::mdns::browse(&service_type, &ifaces, timeout).await
    })
    .await
}
