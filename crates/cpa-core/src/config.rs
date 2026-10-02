//! `config.yaml`, read the way CLIProxyAPI v8 reads it.
//!
//! Both the v8 layout (`server.port`, `access.api-keys`, `oauth.auth-dir`) and the
//! legacy top-level spellings are accepted. When both are present the v8 value wins,
//! including explicit `0`, `false` and empty lists, as in CLIProxyAPI.

use std::path::{Path, PathBuf};

use serde::Deserialize;

pub const DEFAULT_PORT: u16 = 8317;
pub const DEFAULT_AUTH_DIR: &str = "~/.cli-proxy-api";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    /// Bind address. Empty binds all interfaces.
    pub host: String,
    pub port: u16,
    /// Client keys for the proxy API, trimmed and de-duplicated. Empty disables client auth.
    pub api_keys: Vec<String>,
    pub auth_dir: PathBuf,
}

impl Config {
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        let text = std::fs::read_to_string(path)?;
        Self::parse(&text)
    }

    pub fn parse(text: &str) -> anyhow::Result<Self> {
        let raw: Raw = if text.trim().is_empty() {
            Raw::default()
        } else {
            serde_yaml_ng::from_str(text)?
        };
        let server = raw.server.unwrap_or_default();
        let access = raw.access.unwrap_or_default();
        let oauth = raw.oauth.unwrap_or_default();
        let auth_dir = oauth
            .auth_dir
            .or(raw.auth_dir)
            .unwrap_or_else(|| DEFAULT_AUTH_DIR.to_owned());
        Ok(Config {
            host: server.host.or(raw.host).unwrap_or_default(),
            port: server.port.or(raw.port).unwrap_or(DEFAULT_PORT),
            api_keys: normalize_keys(access.api_keys.or(raw.api_keys).unwrap_or_default()),
            auth_dir: expand_home(&auth_dir),
        })
    }
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
struct Raw {
    server: Option<RawServer>,
    access: Option<RawAccess>,
    oauth: Option<RawOAuth>,
    host: Option<String>,
    port: Option<u16>,
    api_keys: Option<Vec<String>>,
    auth_dir: Option<String>,
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
struct RawServer {
    host: Option<String>,
    port: Option<u16>,
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
struct RawAccess {
    api_keys: Option<Vec<String>>,
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
struct RawOAuth {
    auth_dir: Option<String>,
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

fn expand_home(path: &str) -> PathBuf {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    match (path.strip_prefix('~'), home) {
        (Some(""), Some(home)) => home,
        (Some(rest), Some(home)) if rest.starts_with('/') => home.join(&rest[1..]),
        _ => PathBuf::from(path),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn v8_wins_over_legacy_even_when_empty() {
        let cfg = Config::parse(
            "port: 9000\napi-keys: [legacy]\nauth-dir: /legacy\n\
             server: {port: 9100}\naccess: {api-keys: []}\noauth: {auth-dir: /v8}\n",
        )
        .unwrap();
        assert_eq!(cfg.port, 9100);
        assert!(cfg.api_keys.is_empty());
        assert_eq!(cfg.auth_dir, PathBuf::from("/v8"));
    }

    #[test]
    fn legacy_spellings_still_work() {
        let cfg = Config::parse(
            "host: 127.0.0.1\nport: 9000\napi-keys: [' a ', a, '', b]\nauth-dir: /x\n",
        )
        .unwrap();
        assert_eq!(cfg.host, "127.0.0.1");
        assert_eq!(cfg.port, 9000);
        assert_eq!(cfg.api_keys, ["a", "b"]);
        assert_eq!(cfg.auth_dir, PathBuf::from("/x"));
    }

    #[test]
    fn defaults_and_home_expansion() {
        let cfg = Config::parse("").unwrap();
        assert_eq!(cfg.port, DEFAULT_PORT);
        assert_eq!(cfg.host, "");
        let home = PathBuf::from(std::env::var("HOME").unwrap());
        assert_eq!(cfg.auth_dir, home.join(".cli-proxy-api"));
        assert_eq!(expand_home("~"), home);
        assert_eq!(expand_home("~other/x"), PathBuf::from("~other/x"));
    }
}
