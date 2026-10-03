//! The `plugins:` section as the host applies it (internal/pluginhost/config.go,
//! internal/config/plugin_path.go, `PluginInstanceConfig.UnmarshalYAML`).

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde_yaml_ng::{Mapping, Value};

use crate::platform;

/// The YAML a plugin receives when it has no config subtree.
pub const DEFAULT_CONFIG_YAML: &str = "enabled: false\npriority: 0\n";

#[derive(Debug, Clone, PartialEq)]
pub struct RuntimeConfig {
    pub enabled: bool,
    pub dir: PathBuf,
    /// By plugin ID.
    pub items: BTreeMap<String, ItemConfig>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ItemConfig {
    pub id: String,
    pub enabled: bool,
    pub priority: i64,
    /// `store.version` or `store.release-tag`, normalized (no `v`), when valid.
    pub version: String,
    /// The plugin's subtree with `enabled` and `priority` filled in, sent at
    /// register/reconfigure.
    pub config_yaml: Vec<u8>,
}

impl ItemConfig {
    /// Go `defaultRuntimeItemConfig`: a discovered plugin without a config entry.
    pub fn default_for(id: &str) -> Self {
        Self {
            id: id.to_owned(),
            enabled: false,
            priority: 0,
            version: String::new(),
            config_yaml: DEFAULT_CONFIG_YAML.as_bytes().to_vec(),
        }
    }
}

/// yaml.v3 decoding into a Go `bool`: YAML booleans plus the 1.1 spellings it accepts
/// for typed bools (`yes`, `on`, `y`, ...).
pub fn yaml_bool(v: &Value) -> Option<bool> {
    match v {
        Value::Bool(b) => Some(*b),
        Value::String(s) => match s.as_str() {
            "y" | "Y" | "yes" | "Yes" | "YES" | "on" | "On" | "ON" => Some(true),
            "n" | "N" | "no" | "No" | "NO" | "off" | "Off" | "OFF" => Some(false),
            _ => None,
        },
        Value::Tagged(t) => yaml_bool(&t.value),
        _ => None,
    }
}

/// yaml.v3 decoding into a Go `int`.
pub fn yaml_int(v: &Value) -> Option<i64> {
    match v {
        Value::Number(n) => n
            .as_i64()
            .or_else(|| n.as_f64().filter(|f| f.fract() == 0.0).map(|f| f as i64)),
        Value::Tagged(t) => yaml_int(&t.value),
        _ => None,
    }
}

/// yaml.v3 decoding of a scalar into a Go `string`.
pub fn yaml_string(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => n.to_string(),
        Value::Tagged(t) => yaml_string(&t.value),
        _ => String::new(),
    }
}

fn get<'a>(v: &'a Value, key: &str) -> Option<&'a Value> {
    v.as_mapping()?.get(key)
}

/// Go `config.ResolvePluginsDir`: default `plugins`, `~` expanded, cleaned. Relative
/// paths stay relative to the working directory, as in Go.
pub fn resolve_dir(dir: &str) -> Result<PathBuf, String> {
    let dir = dir.trim();
    let dir = if dir.is_empty() { "plugins" } else { dir };
    if let Some(rest) = dir.strip_prefix('~') {
        let home = std::env::var_os("HOME")
            .filter(|h| !h.is_empty())
            .ok_or_else(|| "resolve plugins directory: $HOME is not defined".to_owned())?;
        let rest = rest.trim_start_matches(['/', '\\']);
        let home = PathBuf::from(home);
        if rest.is_empty() {
            return Ok(platform::clean(&home));
        }
        return Ok(platform::clean(&home.join(rest.replace('\\', "/"))));
    }
    Ok(platform::clean(std::path::Path::new(dir)))
}

/// `plugins.dir` from a config document, unresolved.
pub fn configured_dir(document: &Value) -> String {
    get(document, "plugins")
        .and_then(|p| get(p, "dir"))
        .map(yaml_string)
        .unwrap_or_default()
}

/// Go `runtimeConfigFromConfig`.
pub fn runtime_config(document: &Value) -> Result<RuntimeConfig, String> {
    let mut out = RuntimeConfig {
        enabled: false,
        dir: PathBuf::from("plugins"),
        items: BTreeMap::new(),
    };
    let Some(plugins) = get(document, "plugins") else {
        return Ok(out);
    };
    out.enabled = get(plugins, "enabled").and_then(yaml_bool).unwrap_or(false);
    if !out.enabled {
        return Ok(out);
    }
    out.dir = resolve_dir(&configured_dir(document))?;
    if let Some(Value::Mapping(configs)) = get(plugins, "configs") {
        for (key, node) in configs {
            let id = yaml_string(key);
            out.items.insert(id.clone(), item_config(&id, node));
        }
    }
    Ok(out)
}

/// One `plugins.configs.<id>` entry (Go `PluginInstanceConfig.UnmarshalYAML` and
/// `runtimeConfigYAML`). A null entry is the zero config.
pub fn item_config(id: &str, node: &Value) -> ItemConfig {
    let mut enabled = false;
    let mut priority = 0;
    if let Value::Mapping(map) = node {
        if let Some(v) = map.get("enabled") {
            // ponytail: Go fails the whole config load on an undecodable value; the Rust
            // loader already accepted the file, so treat it as disabled.
            enabled = yaml_bool(v).unwrap_or(false);
        }
        if let Some(v) = map.get("priority") {
            priority = yaml_int(v).unwrap_or(0);
        }
    }
    ItemConfig {
        id: id.to_owned(),
        enabled,
        priority,
        version: desired_version(node),
        config_yaml: config_yaml(node, enabled, priority),
    }
}

/// Go `pluginConfigDesiredVersion`.
fn desired_version(node: &Value) -> String {
    let Some(store) = get(node, "store") else {
        return String::new();
    };
    let scalar = |key| {
        get(store, key)
            .map(|v| yaml_string(v).trim().to_owned())
            .unwrap_or_default()
    };
    let version = normalize_version(&scalar("version"));
    if !version.is_empty() {
        return version;
    }
    normalize_version(&scalar("release-tag"))
}

/// Go `normalizePluginDesiredVersion`: strips one leading `v`/`V`, empty when invalid.
pub fn normalize_version(version: &str) -> String {
    let mut version = version.trim();
    if version.len() > 1 && (version.starts_with('v') || version.starts_with('V')) {
        version = &version[1..];
    }
    if platform::valid_version(version) {
        version.to_owned()
    } else {
        String::new()
    }
}

/// Go `runtimeConfigYAML` / `normalizedConfigNode`: the subtree with `enabled` and
/// `priority` appended when missing.
///
/// ponytail: rendered by serde_yaml_ng, so comments and yaml.v3's four-space indent are
/// not reproduced; plugins parse the YAML, so the values are what matters.
fn config_yaml(node: &Value, enabled: bool, priority: i64) -> Vec<u8> {
    let rendered = match node {
        Value::Null => {
            let mut map = Mapping::new();
            map.insert("enabled".into(), enabled.into());
            map.insert("priority".into(), priority.into());
            Value::Mapping(map)
        }
        Value::Mapping(map) => {
            let mut map = map.clone();
            if !map.contains_key("enabled") {
                map.insert("enabled".into(), enabled.into());
            }
            if !map.contains_key("priority") {
                map.insert("priority".into(), priority.into());
            }
            Value::Mapping(map)
        }
        other => other.clone(),
    };
    let text = serde_yaml_ng::to_string(&rendered).unwrap_or_default();
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return DEFAULT_CONFIG_YAML.as_bytes().to_vec();
    }
    let mut out = trimmed.as_bytes().to_vec();
    out.push(b'\n');
    out
}

/// Go `desiredPluginVersions`.
pub fn desired_versions(items: &BTreeMap<String, ItemConfig>) -> BTreeMap<String, String> {
    items
        .iter()
        .filter(|(id, item)| !id.trim().is_empty() && !item.version.trim().is_empty())
        .map(|(id, item)| (id.trim().to_owned(), item.version.trim().to_owned()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn doc(text: &str) -> Value {
        serde_yaml_ng::from_str(text).unwrap()
    }

    /// Go `TestRuntimeConfigYAMLAddsHostDefaultsToRawPluginConfig` and the item rules of
    /// `PluginInstanceConfig.UnmarshalYAML`.
    #[test]
    fn items_follow_go() {
        let rc = runtime_config(&doc(
            "plugins:\n  enabled: true\n  dir: /p\n  configs:\n    a:\n      mode: fast\n    b:\n      enabled: yes\n      priority: 7\n      store:\n        release-tag: v1.2.3\n    c:\n",
        ))
        .unwrap();
        assert!(rc.enabled);
        assert_eq!(rc.dir, PathBuf::from("/p"));
        let a = &rc.items["a"];
        assert!(!a.enabled);
        assert_eq!(
            String::from_utf8_lossy(&a.config_yaml),
            "mode: fast\nenabled: false\npriority: 0\n"
        );
        let b = &rc.items["b"];
        assert!(b.enabled);
        assert_eq!(b.priority, 7);
        assert_eq!(b.version, "1.2.3");
        assert_eq!(String::from_utf8_lossy(&rc.items["c"].config_yaml), DEFAULT_CONFIG_YAML);
    }

    #[test]
    fn disabled_section_keeps_defaults() {
        let rc = runtime_config(&doc("plugins:\n  enabled: false\n  configs:\n    a: {}\n")).unwrap();
        assert!(!rc.enabled);
        assert!(rc.items.is_empty());
        assert_eq!(rc.dir, PathBuf::from("plugins"));
        assert!(!runtime_config(&doc("port: 1\n")).unwrap().enabled);
    }

    #[test]
    fn dir_resolution_matches_go() {
        assert_eq!(resolve_dir("").unwrap(), PathBuf::from("plugins"));
        assert_eq!(resolve_dir(" ./a/../b/ ").unwrap(), PathBuf::from("b"));
        let home = std::env::var("HOME").unwrap();
        assert_eq!(resolve_dir("~/x\\y").unwrap(), PathBuf::from(format!("{home}/x/y")));
        assert_eq!(normalize_version("V2.0"), "2.0");
        assert_eq!(normalize_version("v"), "");
        assert_eq!(normalize_version("vv1"), "");
    }
}
