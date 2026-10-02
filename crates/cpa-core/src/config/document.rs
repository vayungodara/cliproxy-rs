//! Editable document separate from the runtime snapshot. Presence wins over values.
use anyhow::{Context, bail};
use serde_yaml_ng::{Mapping, Value};

use super::{lookup, validate_shape};

// internal/config/config_v8.go buildV8Paths. Move individual mapping leaves, not
// entire legacy structs: v8 siblings must not suppress unrelated legacy siblings.
const PATHS: &[(&str, &str)] = &[
    ("host", "server.host"),
    ("port", "server.port"),
    ("trusted-proxies", "server.trusted-proxies"),
    ("tls", "server.tls"),
    ("commercial-mode", "server.commercial-mode"),
    ("discovery", "server.discovery"),
    ("remote-management", "management"),
    ("credential-concurrency", "credentials.concurrency"),
    ("credential-in-flight", "credentials.in-flight"),
    ("force-model-prefix", "routing.force-model-prefix"),
    ("request-retry", "routing.retry.request-retry"),
    ("max-retry-credentials", "routing.retry.max-retry-credentials"),
    ("max-retry-interval", "routing.retry.max-retry-interval"),
    ("disable-cooling", "routing.cooldown.disable-cooling"),
    ("save-cooldown-status", "routing.cooldown.save-cooldown-status"),
    (
        "transient-error-cooldown-seconds",
        "routing.cooldown.transient-error-cooldown-seconds",
    ),
    ("proxy-url", "requests.proxy-url"),
    ("passthrough-headers", "requests.passthrough-headers"),
    ("nonstream-keepalive-interval", "requests.nonstream-keepalive-interval"),
    ("streaming", "requests.streaming"),
    ("payload", "requests.payload"),
    ("auth-dir", "oauth.auth-dir"),
    ("auth-auto-refresh-workers", "oauth.auth-auto-refresh-workers"),
    ("oauth-model-alias", "oauth.model-alias"),
    ("oauth-excluded-models", "oauth.excluded-models"),
    ("oauth-request-scoped-errors", "oauth.request-scoped-errors"),
    ("oauth-settings", "oauth.settings"),
    ("ws-auth", "oauth.providers.aistudio.ws-auth"),
    ("codex", "oauth.providers.codex"),
    ("codex-header-defaults", "oauth.providers.codex.header-defaults"),
    ("claude", "oauth.providers.claude"),
    ("claude-code", "oauth.providers.claude.claude-code"),
    (
        "disable-claude-cloak-mode",
        "oauth.providers.claude.disable-claude-cloak-mode",
    ),
    ("claude-header-defaults", "oauth.providers.claude.header-defaults"),
    ("antigravity", "oauth.providers.antigravity"),
    (
        "antigravity-signature-cache-enabled",
        "oauth.providers.antigravity.signature-cache-enabled",
    ),
    (
        "antigravity-signature-bypass-strict",
        "oauth.providers.antigravity.signature-bypass-strict",
    ),
    (
        "quota-exceeded.antigravity-credits",
        "oauth.providers.antigravity.antigravity-credits",
    ),
    ("xai", "oauth.providers.xai"),
    ("devin", "oauth.providers.devin"),
    ("disable-image-generation", "multimedia.disable-image-generation"),
    ("gpt-image-2-base-model", "multimedia.gpt-image-2-base-model"),
    ("video-result-auth-cache-ttl", "multimedia.video-result-auth-cache-ttl"),
    ("debug", "observability.logs.debug"),
    ("logging-to-file", "observability.logs.logging-to-file"),
    ("logs-max-total-size-mb", "observability.logs.logs-max-total-size-mb"),
    ("request-log", "observability.logs.request-log"),
    ("error-logs-max-files", "observability.logs.error-logs-max-files"),
    (
        "usage-statistics-enabled",
        "observability.usage.usage-statistics-enabled",
    ),
    (
        "redis-usage-queue-retention-seconds",
        "observability.usage.redis-usage-queue-retention-seconds",
    ),
    ("pprof", "observability.pprof"),
];
const FAMILIES: &[(&str, &str)] = &[
    ("gemini-api-key", "gemini"),
    ("interactions-api-key", "interactions"),
    ("vertex-api-key", "vertex"),
    ("codex-api-key", "codex"),
    ("claude-api-key", "claude"),
    ("xai-api-key", "xai"),
    ("meta-api-key", "meta"),
    ("openai-compatibility", "openai-compatibility"),
];
const SHARED: &[&str] = &[
    "priority",
    "prefix",
    "proxy-url",
    "models",
    "headers",
    "excluded-models",
    "disable-cooling",
    "request-retry",
    "request-scoped-errors",
];

#[derive(Clone)]
pub struct ConfigDocument(Value);

impl ConfigDocument {
    pub fn parse(text: &str) -> anyhow::Result<Self> {
        let root = serde_yaml_ng::from_str::<Value>(text)?;
        let root = match root {
            Value::Mapping(root) => root,
            // Go accepts an empty file on load, but management rejects empty writes.
            Value::Null if text.trim().is_empty() => Mapping::new(),
            _ => bail!("config must be a mapping"),
        };
        Self::from_mapping(root)
    }

    pub(super) fn from_mapping(mut root: Mapping) -> anyhow::Result<Self> {
        validate_shape(&root)?;
        // Historical client spelling precedence is ordered, before provider moves.
        for old in ["oauth.providers.codex", "providers.codex", "codex"] {
            let old = format!("{old}.optimize-multi-agent-v2");
            if let Some(value) = lookup(&root, &old).cloned() {
                remove(&mut root, &old);
                insert_missing(&mut root, "client.codex.optimize-multi-agent-v2", value)?;
            }
        }
        if let Some(keys) = root.get("api-keys").filter(|v| !v.is_mapping()).cloned() {
            root.remove("api-keys");
            insert_missing(&mut root, "access.api-keys", keys)?;
        }
        for (old, current) in PATHS {
            if let Some(mut value) = lookup(&root, old).cloned() {
                if value.is_null() && super::schema::is_struct(current) {
                    value = Value::Mapping(Mapping::new());
                }
                remove(&mut root, old);
                insert_missing(&mut root, current, value)?;
            }
        }
        for (old, family) in FAMILIES {
            if let Some(value) = root.remove(*old) {
                let groups = legacy_groups(value, family)?;
                insert_missing(&mut root, &format!("api-keys.{family}"), groups)?;
            }
        }
        root.insert(Value::from("config-version"), Value::from(8));
        Ok(Self(Value::Mapping(root)))
    }

    pub fn value(&self) -> &Value {
        &self.0
    }
    pub fn into_value(self) -> Value {
        self.0
    }
    pub fn yaml(&self) -> anyhow::Result<String> {
        Ok(serde_yaml_ng::to_string(&self.0)?)
    }

    /// Preserve unaffected comments, key order and scalar styles. Changes are applied
    /// to a private syntax tree and validated again before the destination is touched.
    pub fn render_preserving(&self, original: &str) -> anyhow::Result<String> {
        let file: yaml_edit::YamlFile = original.parse()?;
        let doc = file.document().context("config must be a mapping")?;
        let before: Value = serde_yaml_ng::from_str(original)?;
        sync_mapping(
            &doc.as_mapping().context("config must be a mapping")?,
            self.0.as_mapping().unwrap(),
            before.as_mapping().context("config must be a mapping")?,
        )?;
        let text = file.to_string();
        if serde_yaml_ng::from_str::<Value>(&text)? != self.0 {
            bail!("edited YAML did not round-trip");
        }
        Ok(text)
    }

    /// Go keeps the inode for single-file Docker bind mounts (config_basic.go).
    /// All parsing/normalization must finish before calling this function.
    pub fn write(path: &std::path::Path, text: &str) -> anyhow::Result<()> {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(path)?;
        file.write_all(text.as_bytes())?;
        file.sync_all()?;
        Ok(())
    }

    pub fn get(&self, parts: &[&str]) -> Option<&Value> {
        let mut node = &self.0;
        for part in parts {
            node = node.as_mapping()?.get(*part)?;
        }
        Some(node)
    }

    /// Paths identify mapping keys, never sequence indexes. PATCH recursively merges
    /// mappings and replaces scalars/lists; null is a value, not a deletion.
    pub fn update(&mut self, parts: &[&str], value: Value, patch: bool) -> anyhow::Result<()> {
        let mut dst = &mut self.0;
        for part in parts {
            if part.is_empty() {
                bail!("invalid_path");
            }
            let map = dst.as_mapping_mut().context("invalid_path")?;
            dst = map
                .entry(Value::from(*part))
                .or_insert_with(|| Value::Mapping(Mapping::new()));
        }
        if patch {
            merge(dst, value);
        } else {
            *dst = value;
        }
        Ok(())
    }

    pub fn delete(&mut self, parts: &[&str]) -> bool {
        if parts.is_empty() {
            return false;
        }
        let mut dst = &mut self.0;
        for part in &parts[..parts.len() - 1] {
            let Some(next) = dst.as_mapping_mut().and_then(|m| m.get_mut(*part)) else {
                return false;
            };
            dst = next;
        }
        dst.as_mapping_mut()
            .and_then(|m| m.remove(parts[parts.len() - 1]))
            .is_some()
    }
}

fn sync_mapping(dst: &yaml_edit::Mapping, src: &Mapping, before: &Mapping) -> anyhow::Result<()> {
    for (key, _) in before {
        if !src.contains_key(key) {
            dst.remove(key.as_str().context("config keys must be strings")?);
        }
    }
    for (key, value) in src {
        let key = key.as_str().context("config keys must be strings")?;
        if before.get(key) == Some(value) {
            continue;
        }
        if let (Some(child), Value::Mapping(src)) = (dst.get_mapping(key), value)
            && !src.is_empty()
        {
            sync_mapping(
                &child,
                src,
                before
                    .get(key)
                    .and_then(Value::as_mapping)
                    .context("config must be a mapping")?,
            )?;
        } else {
            // ponytail: new/replaced mappings use YAML-compatible JSON flow syntax.
            // yaml-edit misindents new root block mappings; keep existing mappings
            // byte-stable and revisit block formatting when the library fixes it.
            let text = if value.is_mapping() {
                serde_json::to_string(value)?
            } else {
                serde_yaml_ng::to_string(value)?
            };
            let parsed: yaml_edit::Document = text.parse()?;
            if let Some(map) = parsed.as_mapping() {
                dst.set(key, map);
            } else if let Some(list) = parsed.as_sequence() {
                dst.set(key, list);
            } else {
                dst.set(key, parsed.as_scalar().context("unsupported YAML value")?);
            }
        }
    }
    Ok(())
}

fn merge(dst: &mut Value, src: Value) {
    if let (Value::Mapping(a), Value::Mapping(b)) = (&mut *dst, &src) {
        for (key, value) in b {
            merge(a.entry(key.clone()).or_insert(Value::Null), value.clone());
        }
    } else {
        *dst = src;
    }
}

fn insert_missing(root: &mut Mapping, path: &str, value: Value) -> anyhow::Result<()> {
    let parts: Vec<_> = path.split('.').collect();
    let mut map = root;
    for part in &parts[..parts.len() - 1] {
        if *part == "routing" && map.get(*part).is_some_and(Value::is_null) {
            map.insert(Value::from(*part), Value::Mapping(Mapping::new()));
        }
        map = map
            .entry(Value::from(*part))
            .or_insert_with(|| Value::Mapping(Mapping::new()))
            .as_mapping_mut()
            .context("config parent must be a mapping")?;
    }
    let key = Value::from(parts[parts.len() - 1]);
    if let Some(existing) = map.get_mut(&key) {
        if let (Value::Mapping(dst), Value::Mapping(src)) = (existing, value) {
            for (key, value) in src {
                insert_missing(dst, key.as_str().context("config keys must be strings")?, value)?;
            }
        }
    } else {
        map.insert(key, value);
    }
    Ok(())
}

fn remove(root: &mut Mapping, path: &str) {
    let Some((head, rest)) = path.split_once('.') else {
        root.remove(path);
        return;
    };
    if let Some(Value::Mapping(child)) = root.get_mut(head) {
        remove(child, rest);
        if child.is_empty() {
            root.remove(head);
        }
    }
}

fn legacy_groups(value: Value, family: &str) -> anyhow::Result<Value> {
    if value.is_null() {
        return Ok(Value::Sequence(Vec::new()));
    }
    let entries = value.as_sequence().context("legacy provider keys must be a list")?;
    let mut groups = Vec::new();
    for (i, entry) in entries.iter().enumerate() {
        let mut key = entry.as_mapping().context("provider key must be a mapping")?.clone();
        if family == "openai-compatibility" {
            let keys = key.remove("api-key-entries").unwrap_or(Value::Sequence(Vec::new()));
            key.insert(Value::from("keys"), keys);
            groups.push(Value::Mapping(key));
            continue;
        }
        let mut group = Mapping::new();
        group.insert(Value::from("name"), Value::from(format!("{family}-{}", i + 1)));
        for name in std::iter::once(&"base-url").chain(SHARED) {
            if let Some(value) = key.remove(*name) {
                group.insert(Value::from(*name), value);
            }
        }
        group.insert(Value::from("keys"), Value::Sequence(vec![Value::Mapping(key)]));
        groups.push(Value::Mapping(group));
    }
    Ok(Value::Sequence(groups))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_migration_and_nested_block_sequences_roundtrip() {
        let original = "# keep\nhost: 0.0.0.0\nport: 8317\nremote-management: {allow-remote: true}\nauth-dir: /fake\napi-keys: [fake-client]\nrequest-retry: 3\nrouting:\n  strategy: round-robin\nrequests:\n  payload:\n    default: []\n";
        let mut doc = ConfigDocument::parse(original).unwrap();
        doc.update(
            &["requests", "payload", "default"],
            serde_yaml_ng::from_str("[{models: [{name: '*', protocol: openai}], params: {temperature: 0.7}}]").unwrap(),
            false,
        )
        .unwrap();
        let file: yaml_edit::YamlFile = original.parse().unwrap();
        let before: Value = serde_yaml_ng::from_str(original).unwrap();
        sync_mapping(
            &file.document().unwrap().as_mapping().unwrap(),
            doc.value().as_mapping().unwrap(),
            before.as_mapping().unwrap(),
        )
        .unwrap();
        let text = file.to_string();
        assert_eq!(serde_yaml_ng::from_str::<Value>(&text).unwrap(), *doc.value(), "{text}");
    }

    #[test]
    fn syntax_edits_create_nested_sections_and_empty_lists() {
        let original = "config-version: 8\nrouting:\n  retry:\n    request-retry: 0 # attempts\naccess:\n  api-keys: [fake-client]\n";
        let mut d = ConfigDocument::parse(original).unwrap();
        d.update(
            &[],
            serde_yaml_ng::from_str("routing: {cooldown: {disable-cooling: false}}\naccess: {api-keys: []}\n").unwrap(),
            true,
        )
        .unwrap();
        let file: yaml_edit::YamlFile = original.parse().unwrap();
        let before: Value = serde_yaml_ng::from_str(original).unwrap();
        sync_mapping(
            &file.document().unwrap().as_mapping().unwrap(),
            d.value().as_mapping().unwrap(),
            before.as_mapping().unwrap(),
        )
        .unwrap();
        assert_eq!(
            serde_yaml_ng::from_str::<Value>(&file.to_string()).unwrap(),
            *d.value(),
            "{}",
            file
        );
        assert!(file.to_string().contains("# attempts"));
        let next = file.to_string();
        assert!(d.delete(&["routing", "retry", "request-retry"]));
        let file: yaml_edit::YamlFile = next.parse().unwrap();
        let before: Value = serde_yaml_ng::from_str(&next).unwrap();
        let result = sync_mapping(
            &file.document().unwrap().as_mapping().unwrap(),
            d.value().as_mapping().unwrap(),
            before.as_mapping().unwrap(),
        );
        assert!(result.is_ok(), "{result:?}\n{file}");
        assert_eq!(
            serde_yaml_ng::from_str::<Value>(&file.to_string()).unwrap(),
            *d.value(),
            "{}",
            file
        );
    }

    #[test]
    fn mixed_layout_merges_leaves_and_keeps_presence() {
        let d = ConfigDocument::parse("request-retry: 4\ndisable-cooling: true\nrouting: {retry: {request-retry: null}, cooldown: {disable-cooling: false}}\ncodex: {disable-codex-cloaking: true, optimize-multi-agent-v2: true}\nclient: {codex: {optimize-multi-agent-v2: false}}\noauth: {providers: {codex: {response-steering: true}}}\n").unwrap();
        assert!(d.get(&["routing", "retry", "request-retry"]).unwrap().is_null());
        assert_eq!(
            d.get(&["routing", "cooldown", "disable-cooling"]),
            Some(&Value::Bool(false))
        );
        assert_eq!(
            d.get(&["oauth", "providers", "codex", "disable-codex-cloaking"]),
            Some(&Value::Bool(true))
        );
        assert_eq!(
            d.get(&["client", "codex", "optimize-multi-agent-v2"]),
            Some(&Value::Bool(false))
        );
        assert!(d.get(&["request-retry"]).is_none());
    }

    #[test]
    fn paths_are_mapping_keys_and_patch_null_is_not_deletion() {
        let mut d = ConfigDocument::parse("access: {api-keys: [old]}\n").unwrap();
        assert!(d.update(&["access", "api-keys", "0"], Value::from("x"), false).is_err());
        d.update(&["access"], serde_yaml_ng::from_str("{api-keys: null}").unwrap(), true)
            .unwrap();
        assert!(d.get(&["access", "api-keys"]).unwrap().is_null());
        assert!(d.delete(&["access", "api-keys"]));
        assert!(!d.delete(&["access", "api-keys"]));
    }

    #[test]
    fn legacy_entries_are_not_coalesced() {
        let d = ConfigDocument::parse("claude-api-key: [{api-key: fake1, base-url: http://example.invalid, weight: 0}, {api-key: fake2, base-url: http://example.invalid}]\n").unwrap();
        let groups = d.get(&["api-keys", "claude"]).unwrap().as_sequence().unwrap();
        assert_eq!(groups.len(), 2);
        assert_eq!(groups[0]["keys"][0]["weight"].as_i64(), Some(0));
        assert_eq!(groups[0]["keys"][0]["api-key"].as_str(), Some("fake1"));
    }
}
