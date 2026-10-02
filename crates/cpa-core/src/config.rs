//! `config.yaml`, read the way CLIProxyAPI v8 reads it (internal/config/config_v8.go,
//! config_load.go).
//!
//! Both the v8 layout (`server.port`, `access.api-keys`, `oauth.auth-dir`) and the
//! legacy top-level spellings are accepted. Presence decides: a v8 key that is present
//! wins even when it is `null`, `0`, `false` or empty. A v8 parent that is present but
//! not a mapping is an error, so `access: null` is rejected instead of silently opening
//! the proxy.

use std::path::{Path, PathBuf};

use anyhow::{Context, bail};
use serde::{Deserialize, Serialize};
use serde_yaml_ng::{Mapping, Value};

mod document;
mod schema;
pub use document::ConfigDocument;
pub use schema::validate as validate_config_fields;

/// Default port used only where CLIProxyAPI falls back to it (management base URL).
/// The loader itself leaves an omitted port at 0, as Go does.
pub const DEFAULT_PORT: u16 = 8317;
pub const DEFAULT_AUTH_DIR: &str = "~/.cli-proxy-api";

#[derive(Clone, PartialEq, Eq)]
pub struct Config {
    /// Bind address. Empty binds all interfaces.
    pub host: String,
    /// 0 when omitted, like Go: the listener then gets an ephemeral port.
    pub port: u16,
    /// Client keys for the proxy API, trimmed and de-duplicated. Empty disables client auth.
    pub api_keys: Vec<String>,
    pub auth_dir: PathBuf,
    pub routing: RoutingConfig,
    pub management: ManagementConfig,
    /// Presence-preserving v8 view, including fields not yet wired into executors.
    pub document: Value,
}

impl std::fmt::Debug for Config {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Config")
            .field("host", &self.host)
            .field("port", &self.port)
            .field("routing", &self.routing)
            .finish_non_exhaustive()
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(default, rename_all = "kebab-case")]
pub struct RoutingConfig {
    pub strategy: String,
    pub force_model_prefix: bool,
    pub session_affinity: bool,
    pub session_affinity_ttl: String,
    pub session_affinity_subagents: Option<bool>,
    pub retry: RetryConfig,
    pub cooldown: CooldownConfig,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(default, rename_all = "kebab-case")]
pub struct RetryConfig {
    pub request_retry: i64,
    pub max_retry_credentials: i64,
    pub max_retry_interval: i64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(default, rename_all = "kebab-case")]
pub struct CooldownConfig {
    pub disable_cooling: bool,
    pub save_cooldown_status: bool,
    pub transient_error_cooldown_seconds: i64,
}

// Do not derive Debug: the key must never appear in diagnostic output.
#[derive(Clone, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(default, rename_all = "kebab-case")]
pub struct ManagementConfig {
    pub allow_remote: bool,
    pub secret_key: String,
    pub disable_control_panel: bool,
    pub disable_auto_update_panel: bool,
    pub panel_github_repository: String,
}

impl std::fmt::Debug for ManagementConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ManagementConfig")
            .field("allow_remote", &self.allow_remote)
            .field("disable_control_panel", &self.disable_control_panel)
            .finish_non_exhaustive()
    }
}

impl Default for Config {
    fn default() -> Self {
        Self::parse("{}").expect("empty mapping is valid")
    }
}

const V8_PARENTS: &[&str] = &[
    "server",
    "management",
    "access",
    "credentials",
    "routing",
    "routing.retry",
    "routing.cooldown",
    "requests",
    "oauth",
    "oauth.providers",
    "multimedia",
    "observability",
    "observability.logs",
    "observability.usage",
    "server.tls",
    "server.discovery",
    "server.discovery.interfaces",
    "credentials.concurrency",
    "credentials.in-flight",
    "requests.streaming",
    "requests.payload",
    "client",
    "client.codex",
    "oauth.providers.codex",
    "oauth.providers.claude",
    "oauth.providers.claude.claude-code",
    "oauth.providers.aistudio",
    "oauth.providers.antigravity",
];

impl Config {
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        let text = std::fs::read_to_string(path)?;
        let config = Self::parse(&text)?;
        if !config.management.secret_key.is_empty() && !is_bcrypt(&config.management.secret_key) {
            let hash = bcrypt::hash(&config.management.secret_key, 10)?;
            let file: yaml_edit::YamlFile = text.parse()?;
            let doc = file.document().context("config must be a mapping")?;
            let parent = if lookup(
                &serde_yaml_ng::from_str::<Value>(&text)?
                    .as_mapping()
                    .cloned()
                    .unwrap_or_default(),
                "management.secret-key",
            )
            .is_some()
            {
                "management"
            } else {
                "remote-management"
            };
            doc.get_mapping(parent)
                .context("management must be a mapping")?
                .set("secret-key", hash);
            ConfigDocument::write(path, &file.to_string())?;
            return Self::parse(&file.to_string());
        }
        Ok(config)
    }

    pub fn parse(text: &str) -> anyhow::Result<Self> {
        let root = match serde_yaml_ng::from_str::<Value>(text)? {
            Value::Null if text.trim().is_empty() => Mapping::new(),
            Value::Mapping(m) => m,
            _ => bail!("config must be a mapping"),
        };
        validate_shape(&root)?;
        let document = ConfigDocument::from_mapping(root.clone())?;
        schema::validate(document.value(), false)?;
        let canonical = document.value().as_mapping().expect("document is a mapping");
        let routing = decode_section::<RoutingConfig>(canonical, "routing")?;
        let mut routing = routing;
        routing.retry.max_retry_credentials = routing.retry.max_retry_credentials.max(0);
        let management = decode_section::<ManagementConfig>(canonical, "management")?;
        let pick = |v8: &str, legacy: &str| lookup(&root, v8).or_else(|| lookup(&root, legacy));
        let host = string_or_empty(pick("server.host", "host"), "server.host")?;
        let port = match pick("server.port", "port") {
            None | Some(Value::Null) => 0,
            Some(v) => v
                .as_u64()
                .and_then(|p| u16::try_from(p).ok())
                .context("server.port must be an integer port number")?,
        };
        // A top-level `api-keys` mapping is the v8 upstream key map, not client keys.
        let legacy_keys = lookup(&root, "api-keys").filter(|v| !v.is_mapping());
        let api_keys = match lookup(&root, "access.api-keys").or(legacy_keys) {
            None | Some(Value::Null) => Vec::new(),
            Some(Value::Sequence(seq)) => seq
                .iter()
                .map(|k| {
                    k.as_str()
                        .map(str::to_owned)
                        .context("access.api-keys must be a list of strings")
                })
                .collect::<anyhow::Result<_>>()?,
            Some(_) => bail!("access.api-keys must be a list of strings"),
        };
        let auth_dir = string_or_empty(pick("oauth.auth-dir", "auth-dir"), "oauth.auth-dir")?;
        Ok(Config {
            host,
            port,
            api_keys: normalize_keys(api_keys),
            auth_dir: resolve_auth_dir(&auth_dir),
            routing,
            management,
            document: document.into_value(),
        })
    }
}

pub fn is_bcrypt(key: &str) -> bool {
    key.starts_with("$2a$") || key.starts_with("$2b$") || key.starts_with("$2y$")
}

fn decode_section<T: serde::de::DeserializeOwned + Default>(root: &Mapping, name: &str) -> anyhow::Result<T> {
    match lookup(root, name) {
        None | Some(Value::Null) => Ok(T::default()),
        Some(value) => Ok(serde_yaml_ng::from_value(null_scalars_to_zero(value.clone()))?),
    }
}

// Go YAML decoding null into a non-pointer field leaves its zero/default value.
// Omit nulls when decoding the typed runtime view; the document keeps them intact.
fn null_scalars_to_zero(mut value: Value) -> Value {
    if let Value::Mapping(ref mut map) = value {
        map.retain(|_, v| !v.is_null());
        for (_, v) in map {
            *v = null_scalars_to_zero(v.clone());
        }
    }
    value
}

fn validate_shape(root: &Mapping) -> anyhow::Result<()> {
    for parent in V8_PARENTS {
        match lookup(root, parent) {
            None | Some(Value::Mapping(_)) => {}
            // Routing is shared with the legacy layout, where null means defaults.
            Some(Value::Null) if *parent == "routing" => {}
            Some(_) => bail!("{parent} must be a mapping"),
        }
    }
    if let Some(version) = lookup(root, "config-version")
        && version.as_u64() != Some(8)
    {
        bail!("unsupported config-version (expected 8)");
    }
    Ok(())
}

/// Value at a dotted path, or `None` when any segment is absent or a parent is not a
/// mapping. Present-but-null returns `Some(Value::Null)`.
fn lookup<'a>(root: &'a Mapping, path: &str) -> Option<&'a Value> {
    let mut parts = path.split('.');
    let mut cur = root.get(parts.next()?)?;
    for part in parts {
        cur = cur.as_mapping()?.get(part)?;
    }
    Some(cur)
}

fn string_or_empty(value: Option<&Value>, name: &str) -> anyhow::Result<String> {
    match value {
        None | Some(Value::Null) => Ok(String::new()),
        Some(Value::String(s)) => Ok(s.clone()),
        Some(_) => bail!("{name} must be a string"),
    }
}

fn normalize_keys(keys: Vec<String>) -> Vec<String> {
    let mut out: Vec<String> = Vec::with_capacity(keys.len());
    for key in keys {
        let key = key.trim();
        if !key.is_empty() && !out.iter().any(|k| k == key) {
            out.push(key.to_owned());
        }
    }
    out
}

/// Go's util.ResolveAuthDir: empty means the default, and a leading `~` is the home
/// directory with any following slashes dropped (so `~other/x` is `$HOME/other/x`).
fn resolve_auth_dir(dir: &str) -> PathBuf {
    let dir = if dir.is_empty() { DEFAULT_AUTH_DIR } else { dir };
    let (Some(rest), Some(home)) = (dir.strip_prefix('~'), std::env::var_os("HOME")) else {
        return PathBuf::from(dir);
    };
    let rest = rest.trim_start_matches(['/', '\\']).replace('\\', "/");
    if rest.is_empty() {
        PathBuf::from(home)
    } else {
        PathBuf::from(home).join(rest)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn home() -> PathBuf {
        PathBuf::from(std::env::var("HOME").unwrap())
    }

    #[test]
    fn v8_wins_by_presence_even_when_null_or_empty() {
        let cfg = Config::parse(
            "port: 9000\napi-keys: [legacy]\nauth-dir: /legacy\nhost: legacyhost\n\
             server: {port: 9100, host: null}\naccess: {api-keys: []}\noauth: {auth-dir: /v8}\n\
             remote-management: {allow-remote: true}\nmanagement: {allow-remote: false, disable-auto-update-panel: true}\n",
        )
        .unwrap();
        assert_eq!(cfg.port, 9100);
        assert_eq!(cfg.host, "", "present null beats legacy");
        assert!(cfg.api_keys.is_empty(), "present empty list beats legacy");
        assert_eq!(cfg.auth_dir, PathBuf::from("/v8"));
        assert!(!cfg.management.allow_remote, "present false beats legacy");
        assert!(cfg.management.disable_auto_update_panel);
        let cfg = Config::parse("api-keys: [legacy]\naccess: {api-keys: null}\n").unwrap();
        assert!(cfg.api_keys.is_empty(), "present null list beats legacy");
    }

    #[test]
    fn legacy_spellings_still_work() {
        let cfg = Config::parse("host: 127.0.0.1\nport: 9000\napi-keys: [' a ', a, '', b]\nauth-dir: /x\n").unwrap();
        assert_eq!((cfg.host.as_str(), cfg.port), ("127.0.0.1", 9000));
        assert_eq!(cfg.api_keys, ["a", "b"]);
        assert_eq!(cfg.auth_dir, PathBuf::from("/x"));
    }

    #[test]
    fn null_or_scalar_parents_are_rejected() {
        for (yaml, want) in [
            ("access: null\n", "access must be a mapping"),
            ("access: [a]\n", "access must be a mapping"),
            ("server: 5\n", "server must be a mapping"),
            ("oauth:\n  providers: null\n", "oauth.providers must be a mapping"),
            ("config-version: 7\n", "unsupported config-version (expected 8)"),
            ("- a\n", "config must be a mapping"),
        ] {
            assert_eq!(Config::parse(yaml).unwrap_err().to_string(), want, "{yaml:?}");
        }
        assert!(
            Config::parse("routing: null\nconfig-version: 8\n").is_ok(),
            "routing: null means defaults"
        );
    }

    #[test]
    fn v8_upstream_key_map_is_not_client_keys() {
        let cfg = Config::parse("api-keys:\n  claude:\n    - keys: [{api-key: fake-key}]\n").unwrap();
        assert!(cfg.api_keys.is_empty());
        for yaml in [
            "api-keys: {claude: [{api-key: fake-key}]}",
            "api-keys: {claude: null}",
            "api-keys: {claude: [{keys: null}]}",
            "api-keys: {claude: [{weight: 2, keys: []}]}",
            "api-keys: {claude: [{keys: [{base-url: https://example.invalid}]}]}",
            "api-keys: {claude: [{keys: [{weight: 1000001}]}]}",
        ] {
            assert!(Config::parse(yaml).is_err(), "{yaml}");
        }
        assert!(Config::parse("api-keys: {claude: [{keys: [{weight: 0}]}]}").is_ok());
    }

    #[test]
    fn scheduler_legacy_keys_and_v8_presence_share_one_typed_view() {
        let cfg = Config::parse(
            "request-retry: 7\nmax-retry-credentials: -2\nmax-retry-interval: 41\n\
             disable-cooling: true\nsave-cooldown-status: true\n\
             transient-error-cooldown-seconds: -3\nforce-model-prefix: true\n\
             routing:\n  strategy: fill-first\n  session-affinity: true\n\
             \x20 session-affinity-ttl: 37m\n  session-affinity-subagents: false\n\
             \x20 retry: {request-retry: 0}\n  cooldown: {disable-cooling: false}\n",
        )
        .unwrap();
        assert_eq!(cfg.routing.strategy, "fill-first");
        assert!(cfg.routing.force_model_prefix && cfg.routing.session_affinity);
        assert_eq!(cfg.routing.session_affinity_ttl, "37m");
        assert_eq!(cfg.routing.session_affinity_subagents, Some(false));
        assert_eq!(cfg.routing.retry.request_retry, 0);
        assert_eq!(cfg.routing.retry.max_retry_credentials, 0);
        assert_eq!(cfg.routing.retry.max_retry_interval, 41);
        assert!(!cfg.routing.cooldown.disable_cooling);
        assert!(cfg.routing.cooldown.save_cooldown_status);
        assert_eq!(cfg.routing.cooldown.transient_error_cooldown_seconds, -3);
        let empty = Config::default();
        assert_eq!(empty.routing, RoutingConfig::default());
        assert_eq!(empty.routing.session_affinity_subagents, None);
        let null = Config::parse("request-retry: 7\nrouting: {retry: {request-retry: null}}\n").unwrap();
        assert_eq!(null.routing.retry.request_retry, 0);
    }

    #[test]
    fn defaults_match_go_loader() {
        let cfg = Config::parse("").unwrap();
        assert_eq!((cfg.port, cfg.host.as_str()), (0, ""));
        assert_eq!(cfg.auth_dir, home().join(".cli-proxy-api"));
        assert_eq!(
            Config::parse("auth-dir: ''\n").unwrap().auth_dir,
            home().join(".cli-proxy-api")
        );
        assert_eq!(resolve_auth_dir("~"), home());
        assert_eq!(resolve_auth_dir("~other/x"), home().join("other/x"));
        assert_eq!(resolve_auth_dir("/abs"), PathBuf::from("/abs"));
    }
}
