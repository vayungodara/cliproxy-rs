//! Editable document separate from the runtime snapshot. Presence wins over values.
use anyhow::{Context, bail};
use serde_yaml_ng::{Mapping, Value};

pub use super::text::archive_comments;
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
        let mut root = serde_yaml_ng::from_str::<Value>(text)?;
        // Go migrates the alias-expanded tree (`expandConfigAliases`): an edit next to
        // an inherited setting must keep it rather than shadow the whole mapping.
        super::expand_merges(&mut root)?;
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
    /// yaml.v3's decoding of typed scalars (see [`super::coerce_typed_scalars`]).
    pub(super) fn coerce_typed_scalars(&mut self) {
        super::schema::coerce_typed_scalars(&mut self.0);
    }
    pub fn into_value(self) -> Value {
        self.0
    }
    pub fn yaml(&self) -> anyhow::Result<String> {
        Ok(serde_yaml_ng::to_string(&self.0)?)
    }

    /// Comment-preserving v8 text of `original`: legacy root entries move under their
    /// v8 parents with their own lines. `None` when the text form is unsupported or
    /// would not parse to the migrated value; callers then render from `original`.
    pub fn migrated_text(&self, original: &str) -> Option<String> {
        let mut moves = vec![super::text::Move {
            old: "api-keys",
            new: "access.api-keys",
        }];
        let legacy_client_keys = serde_yaml_ng::from_str::<Value>(original)
            .ok()?
            .get("api-keys")
            .is_some_and(|v| !v.is_mapping());
        if !legacy_client_keys {
            moves.clear();
        }
        moves.extend(
            PATHS
                .iter()
                .filter(|(old, _)| !old.contains('.'))
                .map(|(old, new)| super::text::Move { old, new }),
        );
        let families: Vec<&str> = FAMILIES.iter().map(|(old, _)| *old).collect();
        let text = super::text::migrate(original, &moves, &families, &self.0)?;
        let parsed: Value = serde_yaml_ng::from_str(&text).ok()?;
        (parsed == self.0).then_some(text)
    }

    /// Go's read-time migration archives unrecognised sections as comments. Returns
    /// the removed `(path, value)` pairs; [`ConfigDocument::render_preserving`] callers
    /// append [`archive_comments`] for them.
    pub fn archive_unknown(&mut self) -> Vec<(String, Value)> {
        super::schema::archive_unknown(&mut self.0)
    }

    /// Preserve unaffected comments, key order and scalar styles. Changes are applied
    /// to a private syntax tree and validated again before the destination is touched.
    /// New or retyped root sections are appended in block style (yaml-edit misindents
    /// block values at the root); anything the syntax tree cannot express falls back
    /// to a full block re-emit that keeps the head comment only.
    pub fn render_preserving(&self, original: &str) -> anyhow::Result<String> {
        if let Some(text) = self.try_render(original)
            && serde_yaml_ng::from_str::<Value>(&text).ok().as_ref() == Some(&self.0)
        {
            return Ok(text);
        }
        let mut text: String = original
            .lines()
            .take_while(|l| l.trim_start().starts_with('#') || l.trim().is_empty())
            .map(|l| format!("{l}\n"))
            .collect();
        for (key, value) in self.0.as_mapping().context("config must be a mapping")? {
            text = super::text::append_root(&text, key.as_str().context("config keys must be strings")?, value);
        }
        if serde_yaml_ng::from_str::<Value>(&text)? != self.0 {
            bail!("edited YAML did not round-trip");
        }
        Ok(text)
    }

    fn try_render(&self, original: &str) -> Option<String> {
        let file: yaml_edit::YamlFile = if original.trim().is_empty() {
            "{}".parse().ok()?
        } else {
            original.parse().ok()?
        };
        let doc = file.document()?;
        let root = doc.as_mapping()?;
        let before: Value = match serde_yaml_ng::from_str(original).ok()? {
            Value::Null => Value::Mapping(Mapping::new()),
            v => v,
        };
        let src = self.0.as_mapping()?;
        let before_map = before.as_mapping()?;
        let mut append = Vec::new();
        for (key, value) in src {
            let complex = matches!(value, Value::Mapping(m) if !m.is_empty())
                || matches!(value, Value::Sequence(s) if !s.is_empty());
            let fits = matches!(
                (before_map.get(key), value),
                (Some(Value::Mapping(_)), Value::Mapping(_))
            ) || before_map.get(key) == Some(value);
            if complex && !fits {
                append.push(key.clone());
            }
        }
        let mut kept = src.clone();
        for key in &append {
            kept.remove(key);
            root.remove(key.as_str()?);
        }
        let mut before_kept = before_map.clone();
        for key in &append {
            before_kept.remove(key);
        }
        sync_mapping(&root, &kept, &before_kept).ok()?;
        let mut text = file.to_string();
        if original.trim().is_empty() {
            text = String::new();
        }
        for key in &append {
            text = super::text::append_root(&text, key.as_str()?, &src[key]);
        }
        // Restore the source order for root keys appended at the end.
        Some(text)
    }

    /// JSON writes see TURN credentials redacted; keep omitted secrets when a server
    /// with the same `urls` list is written back (Go `preserveV8TURNSecrets`).
    pub fn preserve_turn_secrets(&mut self, before: &ConfigDocument) {
        const PATH: [&str; 5] = ["oauth", "providers", "codex", "live-media-relay", "ice-servers"];
        let Some(Value::Sequence(previous)) = before.get(&PATH).cloned() else {
            return;
        };
        let mut node = &mut self.0;
        for part in PATH {
            let Some(next) = node.as_mapping_mut().and_then(|m| m.get_mut(part)) else {
                return;
            };
            node = next;
        }
        let Value::Sequence(next) = node else { return };
        let urls = |server: &Value| -> Option<Vec<String>> {
            server
                .get("urls")?
                .as_sequence()?
                .iter()
                .map(|u| match u {
                    Value::String(s) => Some(s.clone()),
                    Value::Number(n) => Some(n.to_string()),
                    Value::Bool(b) => Some(b.to_string()),
                    _ => None,
                })
                .collect()
        };
        let mut matched = vec![false; previous.len()];
        for server in next.iter_mut() {
            let Some(want) = urls(server) else { continue };
            let Some(i) = (0..previous.len()).find(|&i| !matched[i] && urls(&previous[i]).as_ref() == Some(&want))
            else {
                continue;
            };
            matched[i] = true;
            let Some(map) = server.as_mapping_mut() else { continue };
            for name in ["username", "credential"] {
                if !map.contains_key(name)
                    && let Some(secret) = previous[i].get(name)
                {
                    map.insert(Value::from(name), secret.clone());
                }
            }
        }
    }

    /// Go keeps the inode for single-file Docker bind mounts (config_basic.go).
    /// All parsing/normalization must finish before calling this function.
    pub fn write(path: &std::path::Path, text: &str) -> anyhow::Result<()> {
        use std::io::Write;
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create(true).truncate(true);
        // Unix: a new file is private. Windows has no mode bits (Go's mode is ignored).
        #[cfg(unix)]
        std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
        let mut file = options.open(path)?;
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

    /// Go's saver persists typed values (Go's pre-decode defaults for nulls on
    /// non-pointer scalars, strings for scalars in string fields); do the same for each
    /// path a PUT (`written` replaces `parts`) or PATCH (`written` merges into `parts`)
    /// wrote. A PATCH merges mappings key by key, so only its leaves count.
    pub fn typed_projection(&mut self, parts: &[&str], written: &Value, patch: bool) {
        fn leaves(prefix: &mut Vec<String>, value: &Value, patch: bool, out: &mut Vec<Vec<String>>) {
            match value {
                Value::Mapping(map) if patch => {
                    for (key, child) in map {
                        if let Some(key) = key.as_str() {
                            prefix.push(key.to_owned());
                            leaves(prefix, child, patch, out);
                            prefix.pop();
                        }
                    }
                }
                _ => out.push(prefix.clone()),
            }
        }
        let mut paths = Vec::new();
        let mut prefix = parts.iter().map(|p| (*p).to_owned()).collect();
        leaves(&mut prefix, written, patch, &mut paths);
        for path in paths {
            let path: Vec<&str> = path.iter().map(String::as_str).collect();
            super::schema::typed_projection(&mut self.0, &path);
        }
    }

    /// Go `deleteConfigV8Path`: removes the field, then any ancestor mapping that this
    /// removal left empty. Other explicit empty mappings stay.
    pub fn delete(&mut self, parts: &[&str]) -> bool {
        fn remove(node: &mut Value, parts: &[&str]) -> bool {
            let Some(map) = node.as_mapping_mut() else {
                return false;
            };
            let Some((first, rest)) = parts.split_first() else {
                return false;
            };
            if rest.is_empty() {
                return map.remove(*first).is_some();
            }
            let Some(child) = map.get_mut(*first) else {
                return false;
            };
            if !remove(child, rest) {
                return false;
            }
            if child.as_mapping().is_some_and(serde_yaml_ng::Mapping::is_empty) {
                map.remove(*first);
            }
            true
        }
        remove(&mut self.0, parts)
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
        } else if let (Some(seq), Value::Sequence(items), Some(Value::Sequence(old))) =
            (dst.get_sequence(key), value, before.get(key))
            && sync_sequence(&seq, items, old)?
        {
            // Changed mapping items were edited in place; siblings keep their text.
        } else {
            // Nested block values insert correctly; root-level ones are appended by
            // the caller. Empty collections stay `{}`/`[]`.
            let text = match value {
                Value::Mapping(m) if !m.is_empty() => super::text::emit_value(value),
                Value::Sequence(s) if !s.is_empty() => super::text::emit_value(value),
                _ => serde_yaml_ng::to_string(value)?,
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

/// Edits a sequence item by item when every changed item is a mapping that stays a
/// mapping; `false` (nothing touched) otherwise, so the caller replaces the list.
fn sync_sequence(dst: &yaml_edit::Sequence, src: &[Value], before: &[Value]) -> anyhow::Result<bool> {
    if src.len() != before.len() || dst.len() != src.len() {
        return Ok(false);
    }
    let mut edits = Vec::new();
    for (i, (new, old)) in src.iter().zip(before).enumerate() {
        if new == old {
            continue;
        }
        match (new, old, dst.get(i)) {
            (Value::Mapping(new), Value::Mapping(old), Some(yaml_edit::YamlNode::Mapping(node))) if !new.is_empty() => {
                edits.push((node, new, old));
            }
            _ => return Ok(false),
        }
    }
    for (node, new, old) in edits {
        sync_mapping(&node, new, old)?;
    }
    Ok(true)
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
        let text = doc.render_preserving(original).unwrap();
        assert_eq!(serde_yaml_ng::from_str::<Value>(&text).unwrap(), *doc.value(), "{text}");
        assert!(text.starts_with("# keep\n") && !text.contains('{'), "{text}");
    }

    #[test]
    fn migration_moves_legacy_lines_with_their_comments_and_bytes() {
        let original = "# head\nhost: \"\" # any\nport: 8317\nremote-management:\n  # why\n  allow-remote: true\n\
                        auth-dir: '~/.cli-proxy-api'\napi-keys:\n- \"sk-fake\" # client\n# retry note\nrequest-retry: 3\n\
                        debug: false\nrouting:\n  strategy: fill-first\n";
        let doc = ConfigDocument::parse(original).unwrap();
        let text = doc.migrated_text(original).expect("block layout migrates as text");
        assert_eq!(serde_yaml_ng::from_str::<Value>(&text).unwrap(), *doc.value(), "{text}");
        for kept in [
            "  # head\n  host: \"\" # any\n",
            "  # why\n  allow-remote: true\n",
            "  auth-dir: '~/.cli-proxy-api'\n",
            "  - \"sk-fake\" # client\n",
            "routing:\n  strategy: fill-first\n  retry:\n    # retry note\n    request-retry: 3\n",
            "observability:\n  logs:\n    debug: false\n",
        ] {
            assert!(text.contains(kept), "missing {kept:?} in\n{text}");
        }
        // Unsupported syntax declines rather than guessing.
        let anchored = "defaults: &d {a: 1}\nhost: x\n";
        let doc = ConfigDocument::parse("host: x\n").unwrap();
        assert!(doc.migrated_text(anchored).is_none());
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
