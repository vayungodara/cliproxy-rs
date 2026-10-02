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
use serde_yaml_ng::{Mapping, Value};

/// Default port used only where CLIProxyAPI falls back to it (management base URL).
/// The loader itself leaves an omitted port at 0, as Go does.
pub const DEFAULT_PORT: u16 = 8317;
pub const DEFAULT_AUTH_DIR: &str = "~/.cli-proxy-api";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    /// Bind address. Empty binds all interfaces.
    pub host: String,
    /// 0 when omitted, like Go: the listener then gets an ephemeral port.
    pub port: u16,
    /// Client keys for the proxy API, trimmed and de-duplicated. Empty disables client auth.
    pub api_keys: Vec<String>,
    pub auth_dir: PathBuf,
}

// ponytail: parents of the v8 paths this loader reads plus the common v8 sections.
// The config port walks the full v8 struct tree (buildV8Paths) and every leaf.
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
];

impl Config {
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        let text = std::fs::read_to_string(path)?;
        Self::parse(&text)
    }

    pub fn parse(text: &str) -> anyhow::Result<Self> {
        let root = match serde_yaml_ng::from_str::<Value>(text)? {
            Value::Null => Mapping::new(),
            Value::Mapping(m) => m,
            _ => bail!("config must be a mapping"),
        };
        validate_shape(&root)?;
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
        })
    }
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
             server: {port: 9100, host: null}\naccess: {api-keys: []}\noauth: {auth-dir: /v8}\n",
        )
        .unwrap();
        assert_eq!(cfg.port, 9100);
        assert_eq!(cfg.host, "", "present null beats legacy");
        assert!(cfg.api_keys.is_empty(), "present empty list beats legacy");
        assert_eq!(cfg.auth_dir, PathBuf::from("/v8"));
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
        let cfg = Config::parse("api-keys:\n  claude:\n    - api-key: sk-x\n").unwrap();
        assert!(cfg.api_keys.is_empty());
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
