//! LAN discovery over mDNS / DNS-SD (Go `internal/discovery`, `internal/cmd/discover.go`
//! and `sdk/cliproxy/discovery_advertiser.go`): the `discover` command and the server's
//! opt-in `_ai-gateway._tcp` advertisement.
use std::collections::BTreeMap;
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde_json::{Value, json};
use serde_yaml_ng::Value as Yaml;

pub mod advertise;
pub mod cli;
pub mod dns;
pub mod iface;
pub mod mdns;

pub const DEFAULT_SERVICE_TYPE: &str = "_ai-gateway._tcp";
pub const DEFAULT_DOMAIN: &str = "local.";
pub const PRODUCT_CPA: &str = "cliproxyapi";
const DEFAULT_SUBTYPES: [&str; 5] = [
    "_chat-completions",
    "_responses",
    "_messages",
    "_generate-content",
    "_interactions",
];
const DEFAULT_INSTANCE_PREFIX: &str = "CPA-";
const MAX_TXT_RECORD_BYTES: usize = 255;
const MAX_TXT_BYTES: usize = 400;
const MAX_BROWSE_TXT_RECORDS: usize = 64;
const MAX_BROWSE_TXT_BYTES: usize = 16 * 1024;
const MAX_DISCOVERED_ADDRESSES: usize = 32;
const MAX_METADATA_ITEMS: usize = 32;
const MAX_METADATA_ITEM_BYTES: usize = 64;

/// Go `TXTOptions`; [`Default`] is `DefaultTXTOptions`.
#[derive(Clone, Debug)]
pub struct TxtOptions {
    pub version: String,
    pub product: String,
    pub protocols: Vec<String>,
    pub features: Vec<String>,
    pub api_path_openai: String,
    pub api_path_anthropic: String,
    pub api_path_gemini: String,
    pub tls: bool,
    pub auth_required: bool,
    pub auth_methods: Vec<String>,
    pub instance_id: String,
    pub node_role: String,
    pub advertise_management: bool,
}

fn strings(items: &[&str]) -> Vec<String> {
    items.iter().map(|s| (*s).to_owned()).collect()
}

impl Default for TxtOptions {
    fn default() -> Self {
        Self {
            version: "1".into(),
            product: PRODUCT_CPA.into(),
            protocols: strings(&[
                "chat-completions",
                "responses",
                "messages",
                "generate-content",
                "interactions",
            ]),
            features: strings(&["chat", "responses", "messages", "generate_content", "interactions"]),
            api_path_openai: "/v1".into(),
            api_path_anthropic: "/v1".into(),
            api_path_gemini: "/v1beta".into(),
            tls: false,
            auth_required: true,
            auth_methods: strings(&["api_key"]),
            instance_id: String::new(),
            node_role: "standalone".into(),
            advertise_management: false,
        }
    }
}

/// Go `BuildTXTRecords`: printable-ASCII values in priority order, each record at most
/// 255 bytes and the set at most 400 (one length byte per record counted).
pub fn build_txt_records(opts: &TxtOptions) -> Vec<String> {
    let or = |v: &str, d: &str| if v.is_empty() { d.to_owned() } else { v.to_owned() };
    let bool_text = |b: bool, t: &'static str, f: &'static str| if b { t } else { f };
    let candidates = [
        ("version", or(&opts.version, "1")),
        ("product", or(&opts.product, PRODUCT_CPA)),
        ("instance_id", opts.instance_id.clone()),
        ("tls", bool_text(opts.tls, "1", "0").into()),
        ("auth_required", bool_text(opts.auth_required, "true", "false").into()),
        ("api_openai", or(&opts.api_path_openai, "/v1")),
        ("api_anthropic", or(&opts.api_path_anthropic, "/v1")),
        ("api_gemini", or(&opts.api_path_gemini, "/v1beta")),
        (
            "management",
            bool_text(opts.advertise_management, "true", "false").into(),
        ),
        ("auth_methods", opts.auth_methods.join(",")),
        ("protocols", opts.protocols.join(",")),
        ("node_role", opts.node_role.clone()),
        ("features", opts.features.join(",")),
    ];
    let mut records = Vec::new();
    let mut total = 0;
    for (key, value) in candidates {
        let value: String = go_trim(&value).chars().filter(|c| (' '..='~').contains(c)).collect();
        if value.is_empty() {
            continue;
        }
        let entry = format!("{key}={value}");
        if entry.len() > MAX_TXT_RECORD_BYTES || total + entry.len() + 1 > MAX_TXT_BYTES {
            continue;
        }
        total += entry.len() + 1;
        records.push(entry);
    }
    records
}

/// Go `strings.TrimSpace` (Unicode White_Space).
pub(crate) fn go_trim(s: &str) -> &str {
    // Trimming whole whitespace characters keeps valid UTF-8 valid.
    std::str::from_utf8(cpa_core::config::go_trim_space(s.as_bytes())).unwrap_or(s)
}

/// Go `ParseTXTRecords`: keys trimmed and lowercased, last duplicate wins.
pub fn parse_txt_records(txt: &[String]) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    for entry in txt {
        let (key, value) = entry.split_once('=').unwrap_or((entry, ""));
        let key = cpa_common::gostr::lower_bytes(go_trim(key).as_bytes());
        if !key.is_empty() {
            out.insert(key, value.to_owned());
        }
    }
    out
}

fn is_hex4(s: &str) -> bool {
    s.len() == 4 && s.bytes().all(|b| b.is_ascii_hexdigit())
}

/// Go `ResolveDiscoveryStateDir`: `$WRITABLE_PATH/discovery`, else the user config dir.
pub fn state_dir() -> Option<PathBuf> {
    let trimmed = |k: &str| {
        std::env::var(k)
            .ok()
            .map(|v| go_trim(&v).to_owned())
            .filter(|v| !v.is_empty())
    };
    if let Some(w) = trimmed("WRITABLE_PATH").or_else(|| trimmed("writable_path")) {
        return Some(Path::new(&w).join("discovery"));
    }
    let env = |k: &str| std::env::var(k).ok().filter(|v| !v.is_empty());
    let config_dir = if cfg!(target_os = "macos") {
        env("HOME").map(|h| Path::new(&h).join("Library/Application Support"))
    } else if cfg!(windows) {
        env("AppData").map(PathBuf::from)
    } else {
        env("XDG_CONFIG_HOME")
            .filter(|d| Path::new(d).is_absolute())
            .map(PathBuf::from)
            .or_else(|| env("HOME").map(|h| Path::new(&h).join(".config")))
    };
    config_dir.map(|d| d.join("cpa").join("discovery"))
}

static INSTANCE_IDS: Mutex<BTreeMap<PathBuf, String>> = Mutex::new(BTreeMap::new());

/// Go `GetOrGenerateInstanceID`: a persisted 4-hex-digit ID per state dir, cached for
/// the process; generated randomly and written atomically (0600) when absent.
pub fn instance_id(dir: Option<&Path>) -> String {
    let key = dir.map(Path::to_path_buf).unwrap_or_default();
    let mut cache = INSTANCE_IDS.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(id) = cache.get(&key) {
        return id.clone();
    }
    let path = dir.map(|d| d.join("instance_id"));
    if let Some(id) = path
        .as_ref()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .map(|s| go_trim(&s).to_owned())
        .filter(|s| is_hex4(s))
    {
        let id = id.to_uppercase();
        cache.insert(key, id.clone());
        return id;
    }
    let mut buf = [0u8; 2];
    if getrandom::fill(&mut buf).is_err() {
        return format!("{:04X}", std::process::id() & 0xFFFF);
    }
    let id = format!("{:02X}{:02X}", buf[0], buf[1]);
    if let (Some(dir), Some(path)) = (dir, path) {
        let _ = persist(dir, &path, &id);
    }
    cache.insert(key, id.clone());
    id
}

fn persist(dir: &Path, path: &Path, id: &str) -> std::io::Result<()> {
    use std::io::Write;
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
    builder.create(dir)?;
    let mut tmp = dir.join(format!("instance_id_{}.tmp", std::process::id()));
    for n in 0.. {
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
        match options.open(&tmp) {
            Ok(mut file) => {
                file.write_all(id.as_bytes())?;
                file.sync_all()?;
                break;
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists && n < 16 => {
                tmp = dir.join(format!("instance_id_{}_{n}.tmp", std::process::id()));
            }
            Err(e) => return Err(e),
        }
    }
    std::fs::rename(&tmp, path).inspect_err(|_| {
        let _ = std::fs::remove_file(&tmp);
    })
}

/// Go `truncateRunesTo`: at most `max` bytes without splitting a character.
fn truncate_chars(s: &str, max: usize) -> &str {
    let mut end = s.len().min(max);
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

/// Go `sanitizeInstanceName`: control characters dropped, trimmed, at most 63 bytes.
pub fn sanitize_instance_name(name: &str) -> String {
    let clean: String = name.chars().filter(|&c| c >= ' ' && c != '\x7f').collect();
    truncate_chars(go_trim(&clean), 63).to_owned()
}

/// Go `FormatInstanceName`: `CPA-<ID>` or `<name>-<ID>` within the 63-byte label limit.
pub fn format_instance_name(custom: &str, id: &str) -> String {
    let id = if is_hex4(id) { id.to_uppercase() } else { "0001".into() };
    let suffix = format!("-{id}");
    let default = format!("{DEFAULT_INSTANCE_PREFIX}{id}");
    let mut base = sanitize_instance_name(custom);
    if base.is_empty() || base.eq_ignore_ascii_case(&default) {
        return default;
    }
    if base.len() >= suffix.len()
        && base.is_char_boundary(base.len() - suffix.len())
        && base[base.len() - suffix.len()..].eq_ignore_ascii_case(&suffix)
    {
        base = go_trim(&base[..base.len() - suffix.len()]).to_owned();
    }
    let base = go_trim(truncate_chars(&base, 63 - suffix.len()));
    if base.is_empty() {
        default
    } else {
        format!("{base}{suffix}")
    }
}

/// Go `validateServiceType` (RFC 6763 / RFC 6335, TCP only).
pub fn validate_service_type(st: &str) -> Result<(), String> {
    let st = go_trim(st);
    if st.is_empty() {
        return Err("service type cannot be empty".into());
    }
    if st.len() > 63 {
        return Err(format!("service type {} exceeds 63 characters", go_quote(st)));
    }
    let Some(prefix) = st.strip_suffix("._tcp") else {
        return Err(format!("service type {} must end with ._tcp", go_quote(st)));
    };
    let Some(name) = prefix.strip_prefix('_') else {
        return Err(format!("service type {} must start with an underscore", go_quote(st)));
    };
    if name.is_empty() || name.len() > 15 {
        return Err(format!(
            "service name {} must be between 1 and 15 characters (RFC 6335)",
            go_quote(name)
        ));
    }
    for (i, c) in name.char_indices() {
        if !(c.is_ascii_alphanumeric() || c == '-') {
            return Err(format!(
                "service type contains invalid character {} (RFC 6335)",
                go_rune_quote(c)
            ));
        }
        if (i == 0 || i == name.len() - 1) && c == '-' {
            return Err("service name cannot start or end with a hyphen".into());
        }
    }
    Ok(())
}

fn go_quote(s: &str) -> String {
    cpa_common::gostr::quote(s)
}

/// `%q` of a rune.
fn go_rune_quote(c: char) -> String {
    match c {
        '\'' => "'\\''".into(),
        c if cpa_common::gostr::is_print(c) => format!("'{c}'"),
        c => {
            let q = go_quote(&c.to_string());
            format!("'{}'", &q[1..q.len() - 1])
        }
    }
}

/// Go `sanitizeSubtype`: `_label` with RFC 6335 characters, else empty.
pub fn sanitize_subtype(sub: &str) -> String {
    let sub = go_trim(sub);
    if sub.is_empty() || sub.contains('.') {
        return String::new();
    }
    let sub = if sub.starts_with('_') {
        sub.to_owned()
    } else {
        format!("_{sub}")
    };
    let label = &sub[1..];
    let valid = !label.is_empty()
        && label.len() <= 62
        && !label.starts_with('-')
        && !label.ends_with('-')
        && label.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-');
    if valid { sub } else { String::new() }
}

/// `discovery` / `server.discovery` settings as Go's loader leaves them.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct DiscoveryConfig {
    pub enabled: bool,
    pub service_name: String,
    pub service_type: String,
    pub subtypes: Vec<String>,
    pub include: Vec<String>,
    pub exclude: Vec<String>,
    pub auth_required: Option<bool>,
    pub advertise_management: bool,
}

/// A YAML scalar as yaml.v3 decodes it into a Go string (null is empty).
fn scalar(v: &Yaml) -> Option<String> {
    match v {
        Yaml::Null => Some(String::new()),
        Yaml::String(s) => Some(s.clone()),
        Yaml::Bool(b) => Some(b.to_string()),
        Yaml::Number(n) => Some(n.to_string()),
        Yaml::Tagged(t) => scalar(&t.value),
        _ => None,
    }
}

/// A YAML list of strings; `None` when yaml.v3 would fail to decode it.
fn string_list(v: Option<&Yaml>) -> Option<Vec<String>> {
    match v {
        None | Some(Yaml::Null) => Some(Vec::new()),
        Some(Yaml::Sequence(items)) => items.iter().map(scalar).collect(),
        Some(_) => None,
    }
}

impl DiscoveryConfig {
    /// Reads the v8 document (`server.discovery`; legacy `discovery` is migrated there).
    pub fn from_document(doc: &Yaml) -> Self {
        let d = doc.get("server").and_then(|s| s.get("discovery"));
        let get = |k: &str| d.and_then(|d| d.get(k));
        let interfaces = get("interfaces");
        let mut cfg = Self {
            enabled: get("enabled").and_then(Yaml::as_bool).unwrap_or(false),
            service_name: get("service-name").and_then(scalar).unwrap_or_default(),
            service_type: get("service-type").and_then(scalar).unwrap_or_default(),
            subtypes: string_list(get("subtypes")).unwrap_or_default(),
            include: string_list(interfaces.and_then(|i| i.get("include"))).unwrap_or_default(),
            exclude: string_list(interfaces.and_then(|i| i.get("exclude"))).unwrap_or_default(),
            auth_required: get("auth-required").and_then(Yaml::as_bool),
            advertise_management: get("advertise-management").and_then(Yaml::as_bool).unwrap_or(false),
        };
        if cfg.service_type.is_empty() {
            cfg.service_type = DEFAULT_SERVICE_TYPE.into();
        }
        if cfg.subtypes.is_empty() {
            cfg.subtypes = strings(&DEFAULT_SUBTYPES);
        }
        cfg
    }
}

/// Go `ServiceSpec`.
#[derive(Clone, Debug)]
pub struct ServiceSpec {
    pub instance_name: String,
    pub service_type: String,
    pub domain: String,
    pub port: u16,
    pub subtypes: Vec<String>,
    pub text_records: Vec<String>,
    pub interfaces: Vec<iface::Iface>,
    pub advertised_ips: Vec<IpAddr>,
}

impl PartialEq for ServiceSpec {
    /// Go `specEqual`: interfaces compare by index and name only.
    fn eq(&self, o: &Self) -> bool {
        self.instance_name == o.instance_name
            && self.service_type == o.service_type
            && self.domain == o.domain
            && self.port == o.port
            && self.subtypes == o.subtypes
            && self.text_records == o.text_records
            && self.advertised_ips == o.advertised_ips
            && self.interfaces.len() == o.interfaces.len()
            && self
                .interfaces
                .iter()
                .zip(&o.interfaces)
                .all(|(a, b)| a.index == b.index && a.name == b.name)
    }
}

/// Go `BuildServiceSpec`, with the persistent ID (`instance_id(state_dir())`) and the
/// interface filter (`iface::filter`) passed in.
pub fn build_service_spec(
    d: &DiscoveryConfig,
    bind_host: &str,
    port: u16,
    tls: bool,
    instance_id: impl FnOnce() -> String,
    filter: impl FnOnce(&[String], &[String]) -> Result<Vec<iface::Iface>, String>,
) -> Result<ServiceSpec, String> {
    if port == 0 {
        return Err(format!(
            "discovery: invalid service port {port} (must be between 1 and 65535)"
        ));
    }
    let id = instance_id();
    let instance_name = format_instance_name(&d.service_name, &id);
    let service_type = match go_trim(&d.service_type) {
        "" => DEFAULT_SERVICE_TYPE.to_owned(),
        st => {
            validate_service_type(st).map_err(|e| format!("discovery: invalid service-type: {e}"))?;
            st.to_owned()
        }
    };
    let raw: Vec<String> = if d.subtypes.is_empty() {
        strings(&DEFAULT_SUBTYPES)
    } else {
        d.subtypes.clone()
    };
    let mut subtypes: Vec<String> = raw
        .iter()
        .map(|s| sanitize_subtype(s))
        .filter(|s| !s.is_empty())
        .collect();
    if subtypes.is_empty() {
        subtypes.push(DEFAULT_SUBTYPES[0].into());
    }
    let mut ifaces = filter(&d.include, &d.exclude).unwrap_or_else(|e| {
        tracing::warn!("discovery: failed to filter interfaces: {e}");
        Vec::new()
    });
    if ifaces.is_empty() {
        return Err(
            "discovery: no qualified physical interfaces found matching filters (refusing fallback to all interfaces)"
                .into(),
        );
    }
    let bind_host = go_trim(bind_host);
    let bind_ip = if bind_host.is_empty() {
        None
    } else {
        let ip = iface::parse_go_ip(bind_host).ok_or_else(|| {
            format!(
                "discovery: refusing LAN advertising for non-IP bind host {}",
                go_quote(bind_host)
            )
        })?;
        if ip.is_loopback() {
            return Err(format!(
                "discovery: LAN advertising is unavailable for loopback bind host {}",
                go_quote(bind_host)
            ));
        }
        Some(ip).filter(|ip| !ip.is_unspecified())
    };
    if let Some(ip) = bind_ip {
        ifaces.retain(|i| i.addrs.iter().any(|a| iface::ip_equal(*a, ip)));
        if ifaces.is_empty() {
            return Err(format!(
                "discovery: no interface owns bind host {}",
                go_quote(bind_host)
            ));
        }
    }
    let mut advertised_ips = iface::usable_ips(&ifaces);
    if let Some(ip) = bind_ip {
        advertised_ips.retain(|a| iface::ip_equal(*a, ip));
    }
    if advertised_ips.is_empty() {
        return Err(format!(
            "discovery: no advertised address matches bind host {}",
            go_quote(bind_host)
        ));
    }
    let txt = TxtOptions {
        instance_id: id,
        tls,
        advertise_management: d.advertise_management,
        auth_required: d.auth_required.unwrap_or(true),
        ..Default::default()
    };
    Ok(ServiceSpec {
        instance_name,
        service_type,
        domain: DEFAULT_DOMAIN.into(),
        port,
        subtypes,
        text_records: build_txt_records(&txt),
        interfaces: ifaces,
        advertised_ips,
    })
}

/// Go `DiscoveredService`.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct DiscoveredService {
    pub instance_name: String,
    pub service_type: String,
    pub domain: String,
    pub host: String,
    pub port: u16,
    pub ipv4: Vec<IpAddr>,
    pub ipv6: Vec<IpAddr>,
    pub protocols: Vec<String>,
    pub features: Vec<String>,
    pub product: String,
    pub auth_required: bool,
    pub auth_methods: Vec<String>,
    pub endpoints: BTreeMap<String, String>,
    pub node_role: String,
    pub version: String,
    pub raw_txt: BTreeMap<String, String>,
}

impl DiscoveredService {
    /// Go's JSON shape: struct order, `omitempty` where Go has it.
    pub fn to_json(&self) -> Value {
        let mut m = serde_json::Map::new();
        m.insert("instance_name".into(), json!(self.instance_name));
        m.insert("service_type".into(), json!(self.service_type));
        m.insert("domain".into(), json!(self.domain));
        m.insert("host".into(), json!(self.host));
        m.insert("port".into(), json!(self.port));
        let ips = |v: &[IpAddr]| Value::from(v.iter().map(|ip| ip.to_string()).collect::<Vec<_>>());
        m.insert("ipv4".into(), ips(&self.ipv4));
        m.insert("ipv6".into(), ips(&self.ipv6));
        let mut opt = |k: &str, v: Value, keep: bool| {
            if keep {
                m.insert(k.into(), v);
            }
        };
        opt("protocols", json!(self.protocols), !self.protocols.is_empty());
        opt("features", json!(self.features), !self.features.is_empty());
        opt("product", json!(self.product), !self.product.is_empty());
        opt("auth_required", json!(self.auth_required), true);
        opt("auth_methods", json!(self.auth_methods), !self.auth_methods.is_empty());
        opt("endpoints", json!(self.endpoints), !self.endpoints.is_empty());
        opt("node_role", json!(self.node_role), !self.node_role.is_empty());
        opt("version", json!(self.version), !self.version.is_empty());
        opt("raw_txt", json!(self.raw_txt), !self.raw_txt.is_empty());
        Value::Object(m)
    }
}

/// One resolved zeroconf `ServiceEntry`.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Entry {
    pub instance: String,
    pub service: String,
    pub domain: String,
    pub host: String,
    pub port: i64,
    pub ipv4: Vec<IpAddr>,
    pub ipv6: Vec<IpAddr>,
    pub text: Vec<String>,
    /// A zero-length A/AAAA matched this host: zeroconf appends a nil IP, which counts
    /// as "resolved" for emission but is dropped by `filterUsableIPs`.
    pub nil_addr: bool,
}

/// Go `browseEntryWithinLimits`.
pub fn entry_within_limits(e: &Entry) -> bool {
    if e.text.len() > MAX_BROWSE_TXT_RECORDS {
        return false;
    }
    let mut total = 0;
    for record in &e.text {
        if record.len() > MAX_TXT_RECORD_BYTES {
            return false;
        }
        total += record.len() + 1;
        if total > MAX_BROWSE_TXT_BYTES {
            return false;
        }
    }
    true
}

fn usable_ips(ips: &[IpAddr]) -> Vec<IpAddr> {
    ips.iter()
        .filter(|ip| !ip.is_loopback() && !ip.is_unspecified())
        .take(MAX_DISCOVERED_ADDRESSES)
        .copied()
        .collect()
}

/// Go `appendUniqueStrings`.
fn append_unique(dst: &mut Vec<String>, src: &[String]) {
    for value in src {
        let value = go_trim(value);
        if value.is_empty()
            || value.len() > MAX_METADATA_ITEM_BYTES
            || dst.len() >= MAX_METADATA_ITEMS
            || dst.iter().any(|d| d == value)
        {
            continue;
        }
        dst.push(value.to_owned());
    }
}

fn append_unique_ips(dst: &mut Vec<IpAddr>, src: &[IpAddr]) {
    for ip in src {
        if dst.len() < MAX_DISCOVERED_ADDRESSES && !dst.contains(ip) {
            dst.push(*ip);
        }
    }
}

/// Go `parseTXTList`.
pub fn parse_txt_list(mut value: &str) -> Vec<String> {
    let mut out = Vec::new();
    while !value.is_empty() && out.len() < MAX_METADATA_ITEMS {
        let item;
        (item, value) = value.split_once(',').unwrap_or((value, ""));
        let item = go_trim(item);
        if !item.is_empty() && item.len() <= MAX_METADATA_ITEM_BYTES {
            append_unique(&mut out, &[item.to_owned()]);
        }
    }
    out
}

/// Go `sanitizeEndpointPath`.
pub fn sanitize_endpoint_path(p: &str) -> String {
    let p = go_trim(p);
    let bad = !p.starts_with('/')
        || p.starts_with("//")
        || p.contains('\\')
        || p.contains("://")
        || p.contains("..")
        || p.chars().any(|c| c < ' ' || c == '\x7f');
    if bad { String::new() } else { p.to_owned() }
}

/// Go `entryToDiscovered`.
pub fn entry_to_discovered(e: &Entry) -> DiscoveredService {
    let parsed = parse_txt_records(&e.text);
    let get = |k: &str| parsed.get(k).cloned().unwrap_or_default();
    let mut svc = DiscoveredService {
        instance_name: e.instance.clone(),
        service_type: e.service.clone(),
        domain: e.domain.clone(),
        host: e.host.clone(),
        port: u16::try_from(e.port).ok().filter(|p| *p >= 1).unwrap_or(0),
        ipv4: usable_ips(&e.ipv4),
        ipv6: usable_ips(&e.ipv6),
        product: get("product"),
        version: get("version"),
        node_role: get("node_role"),
        auth_required: parsed.get("auth_required").is_some_and(|v| v == "true"),
        auth_methods: parse_txt_list(&get("auth_methods")),
        protocols: parse_txt_list(&get("protocols")),
        features: parse_txt_list(&get("features")),
        ..Default::default()
    };
    for (key, txt) in [
        ("openai", "api_openai"),
        ("anthropic", "api_anthropic"),
        ("gemini", "api_gemini"),
    ] {
        if let Some(path) = parsed
            .get(txt)
            .map(|v| sanitize_endpoint_path(v))
            .filter(|p| !p.is_empty())
        {
            svc.endpoints.insert(key.into(), path);
        }
    }
    svc.raw_txt = parsed;
    svc
}

/// Go `mergeDiscoveredService`.
pub fn merge_discovered(dst: &mut DiscoveredService, src: &DiscoveredService) {
    let take = |d: &mut String, s: &String| {
        if !s.is_empty() {
            d.clone_from(s);
        }
    };
    take(&mut dst.host, &src.host);
    if src.port != 0 {
        dst.port = src.port;
    }
    append_unique_ips(&mut dst.ipv4, &src.ipv4);
    append_unique_ips(&mut dst.ipv6, &src.ipv6);
    take(&mut dst.product, &src.product);
    take(&mut dst.version, &src.version);
    take(&mut dst.node_role, &src.node_role);
    let mut total: usize = dst.raw_txt.iter().map(|(k, v)| k.len() + v.len() + 1).sum();
    for (key, value) in &src.raw_txt {
        if key.is_empty() {
            continue;
        }
        let replaced = match dst.raw_txt.get(key) {
            Some(old) => key.len() + old.len() + 1,
            None if dst.raw_txt.len() >= MAX_BROWSE_TXT_RECORDS => continue,
            None => 0,
        };
        let next = total - replaced + key.len() + value.len() + 1;
        if next > MAX_BROWSE_TXT_BYTES {
            continue;
        }
        dst.raw_txt.insert(key.clone(), value.clone());
        total = next;
    }
    if let Some(v) = dst.raw_txt.get("auth_required") {
        dst.auth_required = go_trim(v).eq_ignore_ascii_case("true");
    }
    append_unique(&mut dst.auth_methods, &src.auth_methods);
    append_unique(&mut dst.protocols, &src.protocols);
    append_unique(&mut dst.features, &src.features);
    for (k, v) in &src.endpoints {
        dst.endpoints.insert(k.clone(), v.clone());
    }
}

#[cfg(test)]
mod tests;
