//! The Claude slice of `config.yaml`, read from the presence-preserving document.
//!
//! API-key credentials read Go's `Config.ForAPIKey` view (the executor scopes `cfg`):
//! `oauth.providers.claude.*` written in the v8 layout is OAuth-only there, while the
//! legacy top-level spellings apply to API keys too.

use cpa_core::config::Config;
use serde_yaml_ng::Value;

#[derive(Debug, Clone, Default)]
pub(crate) struct HeaderDefaults {
    pub user_agent: String,
    pub package_version: String,
    pub runtime_version: String,
    pub os: String,
    pub arch: String,
    pub timeout: String,
    pub timezone: String,
    pub stabilize_device_profile: bool,
}

/// One `claude-api-key` entry after group inheritance (Go `config.ClaudeKey`).
#[derive(Debug, Clone, Default)]
pub(crate) struct ClaudeKey {
    pub api_key: String,
    pub base_url: String,
    pub cloak: Option<Cloak>,
    pub fingerprint_profile: String,
    pub rebuild_mid_system_message: bool,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct Cloak {
    pub mode: String,
    pub strict_mode: bool,
    pub sensitive_words: Vec<String>,
    pub cache_user_id: Option<bool>,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct Settings {
    pub header_defaults: HeaderDefaults,
    pub disable_cloak_mode: bool,
    pub model_level_cooling: bool,
    pub keys: Vec<ClaudeKey>,
}

fn at<'a>(root: &'a Value, path: &[&str]) -> Option<&'a Value> {
    path.iter().try_fold(root, |v, k| v.get(*k))
}
fn text(v: Option<&Value>) -> String {
    v.and_then(Value::as_str).unwrap_or_default().to_owned()
}
fn flag(v: Option<&Value>) -> bool {
    v.and_then(Value::as_bool).unwrap_or(false)
}

impl Settings {
    /// Settings for `credential`. API-key credentials do not see OAuth-scoped fields.
    pub fn from_config(cfg: &Config) -> Self {
        let doc = &cfg.document;
        let mut s = Self {
            keys: keys(doc),
            ..Self::default()
        };
        let claude = at(doc, &["oauth", "providers", "claude"]);
        let h = claude.and_then(|c| c.get("header-defaults"));
        let field = |k: &str| text(h.and_then(|h| h.get(k)));
        s.header_defaults = HeaderDefaults {
            user_agent: field("user-agent"),
            package_version: field("package-version"),
            runtime_version: field("runtime-version"),
            os: field("os"),
            arch: field("arch"),
            timeout: field("timeout"),
            timezone: field("timezone"),
            stabilize_device_profile: flag(h.and_then(|h| h.get("stabilize-device-profile"))),
        };
        s.disable_cloak_mode = flag(claude.and_then(|c| c.get("disable-claude-cloak-mode")));
        s.model_level_cooling = flag(claude.and_then(|c| c.get("model-level-cooling")));
        s
    }

    /// `resolveClaudeKeyConfig`: the config entry whose key (and base URL when both
    /// are set) matches the credential, case-insensitively.
    pub fn key_for(&self, api_key: &str, base_url: &str) -> Option<&ClaudeKey> {
        if api_key.is_empty() {
            return None;
        }
        self.keys.iter().find(|k| {
            k.api_key.trim().eq_ignore_ascii_case(api_key)
                && (base_url.is_empty()
                    || k.base_url.trim().is_empty()
                    || k.base_url.trim().eq_ignore_ascii_case(base_url))
        })
    }
}

/// `api-keys.claude[].keys[]` with the group's base-url inherited.
fn keys(doc: &Value) -> Vec<ClaudeKey> {
    let mut out = Vec::new();
    let groups = at(doc, &["api-keys", "claude"]).and_then(Value::as_sequence);
    for group in groups.into_iter().flatten() {
        let group_base = text(group.get("base-url"));
        for key in group.get("keys").and_then(Value::as_sequence).into_iter().flatten() {
            let base = text(key.get("base-url"));
            let cloak = key.get("cloak").filter(|c| c.is_mapping()).map(|c| Cloak {
                mode: text(c.get("mode")),
                strict_mode: flag(c.get("strict-mode")),
                sensitive_words: c
                    .get("sensitive-words")
                    .and_then(Value::as_sequence)
                    .into_iter()
                    .flatten()
                    .filter_map(Value::as_str)
                    .map(str::to_owned)
                    .collect(),
                cache_user_id: c.get("cache-user-id").and_then(Value::as_bool),
            });
            out.push(ClaudeKey {
                api_key: text(key.get("api-key")),
                base_url: if base.is_empty() { group_base.clone() } else { base },
                cloak,
                fingerprint_profile: text(key.get("fingerprint-profile")),
                rebuild_mid_system_message: flag(key.get("rebuild-mid-system-message")),
            });
        }
    }
    out
}
