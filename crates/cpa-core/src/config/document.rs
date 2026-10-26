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
    ("max-request-body-bytes", "server.max-request-body-bytes"),
    ("trusted-proxies", "server.trusted-proxies"),
    ("github-token", "server.github-token"),
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
    (
        "tls-handshake-timeout-seconds",
        "requests.tls-handshake-timeout-seconds",
    ),
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
    ("codex.disable-codex-cloaking", "upstream.codex.disable-codex-cloaking"),
    (
        "codex.stream-bootstrap-buffering",
        "upstream.codex.stream-bootstrap-buffering",
    ),
    (
        "codex.stream-bootstrap-timeout",
        "upstream.codex.stream-bootstrap-timeout",
    ),
    (
        "codex.orphan-delegation-compatibility",
        "upstream.codex.orphan-delegation-compatibility",
    ),
    ("codex.model-level-cooling", "upstream.codex.model-level-cooling"),
    ("codex.response-steering", "upstream.codex.response-steering"),
    ("codex", "oauth.providers.codex"),
    ("codex-header-defaults", "oauth.providers.codex.header-defaults"),
    // Go walks the declared fields of ClaudeConfig and ClaudeCode.
    ("claude.model-level-cooling", "upstream.claude.model-level-cooling"),
    ("claude.tool-search", "upstream.claude.tool-search"),
    ("claude.session-date-pin-days", "upstream.claude.session-date-pin-days"),
    (
        "claude-code.disable-cloaking-model-list",
        "upstream.claude.disable-cloaking-model-list",
    ),
    ("disable-claude-cloak-mode", "upstream.claude.disable-claude-cloak-mode"),
    ("claude-header-defaults", "upstream.claude.header-defaults"),
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
    ("xai", "upstream.xai"),
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
// Go `v8ClientPaths`: the first historical spelling present wins.
const CLIENT_PATHS: &[(&str, &str)] = &[
    (
        "oauth.providers.codex.optimize-multi-agent-v2",
        "client.codex.optimize-multi-agent-v2",
    ),
    (
        "providers.codex.optimize-multi-agent-v2",
        "client.codex.optimize-multi-agent-v2",
    ),
    ("codex.optimize-multi-agent-v2", "client.codex.optimize-multi-agent-v2"),
];
// Go `v8SharedPaths` (52d5507d): historical v8 spellings of the shared `upstream`
// settings. A canonical path wins by presence; these precede the legacy roots.
pub(super) const SHARED_PATHS: &[(&str, &str)] = &[
    (
        "oauth.providers.codex.disable-codex-cloaking",
        "upstream.codex.disable-codex-cloaking",
    ),
    (
        "oauth.providers.codex.stream-bootstrap-buffering",
        "upstream.codex.stream-bootstrap-buffering",
    ),
    (
        "oauth.providers.codex.stream-bootstrap-timeout",
        "upstream.codex.stream-bootstrap-timeout",
    ),
    (
        "oauth.providers.codex.orphan-delegation-compatibility",
        "upstream.codex.orphan-delegation-compatibility",
    ),
    (
        "oauth.providers.codex.model-level-cooling",
        "upstream.codex.model-level-cooling",
    ),
    (
        "oauth.providers.codex.response-steering",
        "upstream.codex.response-steering",
    ),
    (
        "oauth.providers.claude.model-level-cooling",
        "upstream.claude.model-level-cooling",
    ),
    (
        "oauth.providers.claude.claude-code.disable-cloaking-model-list",
        "upstream.claude.disable-cloaking-model-list",
    ),
    (
        "oauth.providers.claude.disable-claude-cloak-mode",
        "upstream.claude.disable-claude-cloak-mode",
    ),
    (
        "oauth.providers.claude.header-defaults.user-agent",
        "upstream.claude.header-defaults.user-agent",
    ),
    (
        "oauth.providers.claude.header-defaults.package-version",
        "upstream.claude.header-defaults.package-version",
    ),
    (
        "oauth.providers.claude.header-defaults.runtime-version",
        "upstream.claude.header-defaults.runtime-version",
    ),
    (
        "oauth.providers.claude.header-defaults.os",
        "upstream.claude.header-defaults.os",
    ),
    (
        "oauth.providers.claude.header-defaults.arch",
        "upstream.claude.header-defaults.arch",
    ),
    (
        "oauth.providers.claude.header-defaults.timeout",
        "upstream.claude.header-defaults.timeout",
    ),
    (
        "oauth.providers.claude.header-defaults.timezone",
        "upstream.claude.header-defaults.timezone",
    ),
    (
        "oauth.providers.claude.header-defaults.stabilize-device-profile",
        "upstream.claude.header-defaults.stabilize-device-profile",
    ),
    ("oauth.providers.xai.inject-x-search", "upstream.xai.inject-x-search"),
];
// Go `v8SharedStructPaths`: an empty or null historical container is an empty
// `upstream` one; populated ones move through their fields.
pub(super) const SHARED_STRUCTS: &[(&str, &str)] = &[
    (
        "oauth.providers.claude.header-defaults",
        "upstream.claude.header-defaults",
    ),
    ("oauth.providers.claude.claude-code", "upstream.claude"),
    ("oauth.providers.claude", "upstream.claude"),
    ("oauth.providers.xai", "upstream.xai"),
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

/// The v8 path a legacy path (as [`super::binary`] walks it: a key, or `None` for a list
/// item, per step) moves to in [`ConfigDocument::from_mapping`]; a v8 path as it is.
pub(super) fn v8_path<'a>(keys: &[Option<&'a str>]) -> Vec<Option<&'a str>> {
    let mut out = Vec::with_capacity(keys.len() + 3);
    match keys {
        [Some(root), None, rest @ ..] if let Some((_, family)) = FAMILIES.iter().find(|(old, _)| old == root) => {
            out.extend([Some("api-keys"), Some(*family), None]);
            match rest.first() {
                Some(Some("api-key-entries")) if *family == "openai-compatibility" => {
                    out.push(Some("keys"));
                    out.extend(&rest[1..]);
                    return out;
                }
                Some(Some(field))
                    if *family != "openai-compatibility" && *field != "base-url" && !SHARED.contains(field) =>
                {
                    out.extend([Some("keys"), None]);
                }
                _ => {}
            }
            out.extend(rest);
        }
        [Some("api-keys"), None, ..] => {
            out.push(Some("access"));
            out.extend(keys);
        }
        _ => {
            /// The moves split into steps.
            static MOVES: std::sync::LazyLock<Vec<(Vec<&str>, Vec<&str>)>> = std::sync::LazyLock::new(|| {
                PATHS
                    .iter()
                    .chain(CLIENT_PATHS)
                    .chain(SHARED_PATHS)
                    .chain(SHARED_STRUCTS)
                    .map(|(old, current)| (old.split('.').collect(), current.split('.').collect()))
                    .collect()
            });
            // Most paths start at a root no move names.
            let moved = MOVES
                .iter()
                .filter(|(old, _)| keys.first() == Some(&Some(old[0])))
                .filter(|(old, _)| old.len() <= keys.len() && old.iter().zip(keys).all(|(o, k)| *k == Some(*o)))
                .max_by_key(|(old, _)| old.len());
            match moved {
                Some((old, current)) => {
                    out.extend(current.iter().map(|step| Some(*step)));
                    out.extend(&keys[old.len()..]);
                }
                None => out.extend(keys),
            }
        }
    }
    out
}

#[derive(Clone)]
pub struct ConfigDocument(Value);

impl ConfigDocument {
    /// The document a write edits and saves: string settings written as plain numbers or
    /// booleans hold their text ([`super::config_value`]), so a save that re-emits them
    /// keeps what Go reads.
    pub fn parse(text: &str) -> anyhow::Result<Self> {
        Self::read(super::config_value(text)?, text)
    }

    /// The document a read serves: the file's YAML types ([`super::yaml_value`]), as Go's
    /// views read the file's nodes (`github-token: 5` reads as the number 5).
    pub fn view(text: &str) -> anyhow::Result<Self> {
        Self::read(super::yaml_value(text)?, text)
    }

    fn read(mut root: Value, text: &str) -> anyhow::Result<Self> {
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
        move_aliases(&mut root)?;
        if let Some(keys) = root.get("api-keys").filter(|v| !v.is_mapping()).cloned() {
            root.remove("api-keys");
            insert_missing(&mut root, "access.api-keys", keys)?;
        }
        // The legacy Claude structs move by their leaves (PATHS); an empty or null one is
        // an empty `upstream.claude`, and other children stay as unknown fields.
        for old in ["claude", "claude-code"] {
            match root.get(old) {
                Some(Value::Null) => {}
                Some(Value::Mapping(m)) if m.is_empty() => {}
                Some(Value::Mapping(_)) | None => continue,
                Some(_) => bail!("{old} must be a mapping"),
            }
            root.remove(old);
            insert_missing(&mut root, "upstream.claude", Value::Mapping(Mapping::new()))?;
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

    /// A document kept in the layout it was written in, for a save that must not
    /// migrate it (Go's v0 save of a file without v8 sections).
    pub fn unmigrated(value: Value) -> Self {
        Self(value)
    }

    pub fn value(&self) -> &Value {
        &self.0
    }
    /// The document for edits a path update cannot express, such as removing a field
    /// from every list entry.
    pub fn value_mut(&mut self) -> &mut Value {
        &mut self.0
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
    /// v8 parents with their own lines. Moves the text form cannot make (a dotted
    /// legacy leaf such as `codex.response-steering`) are left for
    /// [`ConfigDocument::render_preserving`], so the result only has to normalize to
    /// this document. `None` when the text form is unsupported or normalizes to
    /// something else; callers then render from `original`.
    pub fn migrated_text(&self, original: &str) -> Option<String> {
        let mut moves = vec![super::text::Move {
            old: "api-keys",
            new: "access.api-keys",
        }];
        let legacy_client_keys = super::parse_yaml(original)
            .ok()?
            .get("api-keys")
            .is_some_and(|v| !v.is_mapping());
        if !legacy_client_keys {
            moves.clear();
        }
        // Text moves in `from_mapping`'s order; aliases go first.
        let aliases = CLIENT_PATHS.iter().chain(SHARED_PATHS);
        let mut moves: Vec<_> = aliases
            .map(|(old, new)| super::text::Move { old, new })
            .chain(moves)
            .collect();
        moves.extend(PATHS.iter().map(|(old, new)| super::text::Move { old, new }));
        let families: Vec<&str> = FAMILIES.iter().map(|(old, _)| *old).collect();
        let text = super::text::migrate(original, &moves, &families, &self.0)?;
        (Self::parse(&text).ok()?.0 == self.0).then_some(text)
    }

    /// Go `ProjectV8ConfigAliases`: moves canonical values into the historical subtree
    /// a request names, so a read sees them there and a write there replaces or
    /// deletes them. Populated canonical containers stay; their fields move one by one.
    pub fn project_aliases(&mut self, parts: &[&str]) {
        if parts.is_empty() {
            return;
        }
        let path = parts.join(".");
        let Some(root) = self.0.as_mapping_mut() else {
            return;
        };
        for (old, current) in CLIENT_PATHS.iter().chain(SHARED_PATHS).chain(SHARED_STRUCTS) {
            if path != *old && !old.starts_with(&format!("{path}.")) && !path.starts_with(&format!("{old}.")) {
                continue;
            }
            let Some(value) = lookup(root, current).cloned() else {
                continue;
            };
            if value.as_mapping().is_some_and(|m| !m.is_empty()) {
                continue;
            }
            set_path(root, old, value);
            remove(root, current);
        }
    }

    /// Go `NormalizeV8ConfigAliases` for a request body or the edited document: the
    /// historical v8 spellings move to their canonical paths. A null historical
    /// container resets its fields, as null canonical leaves, so a PATCH merge cannot
    /// turn it into a no-op. Unlike the loader's [`move_aliases`], nothing is rejected
    /// here: a missing canonical field replaces a non-mapping ancestor, as Go's
    /// `setYAMLPath` does, and a malformed historical container stays for the config
    /// parse to report (422).
    pub fn normalize_aliases(value: &mut Value) {
        let Some(root) = value.as_mapping_mut() else {
            return;
        };
        for (container, _) in SHARED_STRUCTS {
            if !matches!(lookup(root, container), Some(Value::Null)) {
                continue;
            }
            let prefix = format!("{container}.");
            for (old, current) in CLIENT_PATHS.iter().chain(SHARED_PATHS) {
                if old.starts_with(&prefix) && lookup(root, current).is_none() {
                    set_path(root, current, Value::Null);
                }
            }
            remove(root, container);
        }
        for (old, current) in CLIENT_PATHS.iter().chain(SHARED_PATHS) {
            if let Some(value) = lookup(root, old).cloned() {
                if lookup(root, current).is_none() {
                    set_path(root, current, value);
                }
                remove(root, old);
            }
        }
        for (old, current) in SHARED_STRUCTS {
            match lookup(root, old) {
                Some(Value::Null) => {}
                Some(Value::Mapping(m)) if m.is_empty() => {}
                _ => continue,
            }
            if lookup(root, current).is_none() {
                set_path(root, current, Value::Mapping(Mapping::new()));
            }
            remove(root, old);
        }
    }

    /// Go's read-time migration archives unrecognised sections as comments. Returns
    /// the removed `(path, value)` pairs; [`ConfigDocument::render_preserving`] callers
    /// append [`archive_comments`] for them.
    pub fn archive_unknown(&mut self) -> Vec<(String, Value)> {
        let archived = super::schema::archive_unknown(&mut self.0);
        for (section, _) in &archived {
            // Go `warnUnrecognizedV8Section`.
            tracing::warn!("unrecognized configuration section {section:?} commented out during v8 migration");
        }
        archived
    }

    /// Go `NormalizeConfigLayout(data, false)`, as the startup load runs it: `text`
    /// without the legacy fields that a v8 field overrides (presence wins), or `None`
    /// when it has none. A legacy-only document is not migrated.
    pub fn without_conflicts(text: &str) -> anyhow::Result<Option<String>> {
        let mut raw = super::config_value(text)?;
        super::expand_merges(&mut raw)?;
        let Some(root) = raw.as_mapping_mut() else {
            return Ok(None);
        };
        let mut changed = false;
        // Go checks each pair against the document as earlier deletions left it.
        for (old, new) in conflict_candidates(root) {
            let legacy = lookup(root, &old).is_some_and(|v| !(old == "api-keys" && v.is_mapping()));
            if legacy && lookup(root, &new).is_some() {
                changed |= delete_pruning(root, &old.split('.').collect::<Vec<_>>());
            }
        }
        if !changed {
            return Ok(None);
        }
        Ok(Some(ConfigDocument(raw).render_preserving(text)?))
    }

    /// Preserve unaffected comments, key order and scalar styles. Changes are applied
    /// to a private syntax tree and validated again before the destination is touched.
    /// New or retyped root sections are appended in block style (yaml-edit misindents
    /// block values at the root); anything the syntax tree cannot express falls back
    /// to a full block re-emit that keeps the head comment only.
    pub fn render_preserving(&self, original: &str) -> anyhow::Result<String> {
        // A number a JSON body set in a string setting reads back as its text.
        let mut expected = self.0.clone();
        super::binary::as_reread(&mut expected);
        if let Some(text) = self.try_render(original)
            && super::config_value(&text).ok().as_ref() == Some(&expected)
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
        if super::config_value(&text)? != expected {
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
        let before = match super::config_value(original).ok()? {
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

/// Go's `buildV8Paths` output for v8.0.35, in its order: the declared legacy leaves
/// and structs, each with its v8 path. Regenerate with
/// `tests/reference/legacy_paths/main.go`.
#[derive(serde::Deserialize)]
struct LegacyPaths {
    leaves: Vec<(String, String)>,
    structs: Vec<(String, String)>,
}

static LEGACY: std::sync::LazyLock<LegacyPaths> = std::sync::LazyLock::new(|| {
    serde_json::from_str(include_str!("legacy_paths.json")).expect("legacy_paths.json is valid")
});

/// The (legacy, v8) pairs Go's non-migrating `NormalizeConfigLayout` checks, in its
/// order: the private-IP alias, the historical v8 aliases, Go's declared legacy
/// leaves, the empty or null legacy structs whose v8 struct is present, then the key
/// families. A same-spelled field Go does not declare is not an override.
fn conflict_candidates(root: &Mapping) -> Vec<(String, String)> {
    let pair = |old: &str, new: &str| (old.to_owned(), new.to_owned());
    let mut out = vec![pair(
        "codex.live-media-relay.allow-private-remote-ips",
        "oauth.providers.codex.live-media-relay.disable-private-remote-ips",
    )];
    out.extend(CLIENT_PATHS.iter().chain(SHARED_PATHS).map(|(old, new)| pair(old, new)));
    out.extend(LEGACY.leaves.iter().cloned());
    let structs = LEGACY.structs.iter().map(|(old, new)| (old.as_str(), new.as_str()));
    for (old, new) in structs.chain(SHARED_STRUCTS.iter().copied()) {
        let empty = match lookup(root, old) {
            Some(Value::Null) => true,
            Some(Value::Mapping(m)) => m.is_empty(),
            _ => false,
        };
        if empty && lookup(root, new).is_some() {
            out.push(pair(old, new));
        }
    }
    out.extend(
        FAMILIES
            .iter()
            .map(|(old, family)| ((*old).to_owned(), format!("api-keys.{family}"))),
    );
    out
}

/// Go `deleteYAMLPath`: removes `parts` and every parent mapping the removal empties.
fn delete_pruning(map: &mut Mapping, parts: &[&str]) -> bool {
    let Some((first, rest)) = parts.split_first() else {
        return false;
    };
    if rest.is_empty() {
        return map.shift_remove(*first).is_some();
    }
    let Some(child) = map.get_mut(*first).and_then(Value::as_mapping_mut) else {
        return false;
    };
    if !delete_pruning(child, rest) {
        return false;
    }
    if child.is_empty() {
        map.shift_remove(*first);
    }
    true
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

/// Go `NormalizeV8ConfigAliases`' moves: historical client and shared spellings go
/// to their canonical paths, where a present value wins; an empty or null historical
/// container becomes an empty canonical one.
fn move_aliases(root: &mut Mapping) -> anyhow::Result<()> {
    for (old, current) in CLIENT_PATHS.iter().chain(SHARED_PATHS) {
        if let Some(value) = lookup(root, old).cloned() {
            remove(root, old);
            insert_missing(root, current, value)?;
        }
    }
    for (old, current) in SHARED_STRUCTS {
        match lookup(root, old) {
            None => continue,
            Some(Value::Null) => {}
            Some(Value::Mapping(m)) if m.is_empty() => {}
            // Fields the schema does not know stay for validation to report.
            Some(Value::Mapping(_)) => continue,
            Some(_) => bail!("{old} must be a mapping"),
        }
        remove(root, old);
        insert_missing(root, current, Value::Mapping(Mapping::new()))?;
    }
    Ok(())
}

/// Sets `path`, replacing any value there and creating missing parent mappings.
fn set_path(root: &mut Mapping, path: &str, value: Value) {
    let parts: Vec<_> = path.split('.').collect();
    let mut map = root;
    for part in &parts[..parts.len() - 1] {
        let next = map
            .entry(Value::from(*part))
            .or_insert_with(|| Value::Mapping(Mapping::new()));
        if !next.is_mapping() {
            *next = Value::Mapping(Mapping::new());
        }
        map = next.as_mapping_mut().expect("just made a mapping");
    }
    map.insert(Value::from(parts[parts.len() - 1]), value);
}

/// Go `IsV8ConfigLayout`: a document is v8 by its version, a v8-only section, grouped
/// `api-keys`, or a v8 path; roots shared with the legacy layout do not count alone.
/// The layout checks of Go's `ValidateV8Config` that come before its strict decode, in
/// its order, with its text: a legacy field (Go's declared legacy leaves, the historical
/// v8 aliases, the shared structs), a root section v8 does not have, then an
/// `api-keys` provider that is not a key family. `root` is the merge-expanded document.
pub fn v8_layout_error(root: &Value) -> Option<String> {
    let map = root.as_mapping()?;
    let leaves = LEGACY.leaves.iter().map(|(old, new)| (old.as_str(), new.as_str()));
    let aliases = CLIENT_PATHS.iter().chain(SHARED_PATHS).chain(SHARED_STRUCTS).copied();
    for (old, new) in leaves.chain(aliases) {
        // Go `legacyPath`: an `api-keys` mapping is the v8 key map.
        if lookup(map, old).is_some_and(|v| !(old == "api-keys" && v.is_mapping())) {
            return Some(format!("legacy field {old} is not accepted by v8; use {new}"));
        }
    }
    // Go `v8AllowedRoots`.
    let fixed = [
        "models",
        "config-version",
        "api-keys",
        "plugins",
        "quota-exceeded",
        "client",
    ];
    for key in map.keys().filter_map(Value::as_str) {
        let section = |new: &String| new.split('.').next() == Some(key);
        if !fixed.contains(&key) && !LEGACY.leaves.iter().any(|(_, new)| section(new)) {
            return Some(format!("unknown v8 configuration section {key}"));
        }
    }
    let groups = map.get("api-keys").and_then(Value::as_mapping)?;
    groups
        .keys()
        .filter_map(Value::as_str)
        .find(|key| !FAMILIES.iter().any(|(_, family)| family == key))
        .map(|key| format!("unknown API-key provider {key}"))
}

/// Go `flattenV8`'s shape checks, in its order: every parent of a v8 path, then of a
/// historical alias's destination, must be a mapping (`routing` may also be null, as
/// in the legacy layout); then each shared container (`v8SharedStructPaths`) must be a
/// mapping or null. `root` is the merge-expanded document.
pub(super) fn shape_error(root: &Mapping) -> Option<String> {
    static PARENTS: std::sync::LazyLock<Vec<String>> = std::sync::LazyLock::new(|| {
        let aliases = CLIENT_PATHS.iter().chain(SHARED_PATHS).map(|(_, current)| *current);
        let mut out: Vec<String> = Vec::new();
        for current in LEGACY.leaves.iter().map(|(_, current)| current.as_str()).chain(aliases) {
            for (i, _) in current.match_indices('.') {
                if !out.iter().any(|p| *p == current[..i]) {
                    out.push(current[..i].to_owned());
                }
            }
        }
        out
    });
    for parent in PARENTS.iter() {
        match lookup(root, parent) {
            None | Some(Value::Mapping(_)) => {}
            Some(Value::Null) if parent == "routing" => {}
            Some(_) => return Some(format!("{parent} must be a mapping")),
        }
    }
    SHARED_STRUCTS.iter().find_map(|(old, _)| match lookup(root, old) {
        None | Some(Value::Null | Value::Mapping(_)) => None,
        Some(_) => Some(format!("{old} must be a mapping")),
    })
}

/// The credential checks Go's `validateCredentialWeightYAML` makes (first in
/// `LoadConfig` and `ParseConfigBytes`, unwrapped), after `flattenV8`'s shape and
/// version checks: [`v8_group_error`], then the weights of the legacy key lists and
/// OpenAI-compatible `api-key-entries` a v8 family does not replace, in document order.
/// `root` is the merge-expanded document.
pub fn credential_error(root: &Mapping) -> Option<String> {
    if let Some(e) = v8_group_error(root) {
        return Some(e);
    }
    let groups = root.get("api-keys").and_then(Value::as_mapping);
    root.iter().find_map(|(name, value)| {
        let name = name.as_str()?;
        let (_, family) = FAMILIES.iter().find(|(legacy, _)| *legacy == name)?;
        if groups.is_some_and(|g| g.contains_key(*family)) {
            return None;
        }
        if name != "openai-compatibility" {
            return weight_error(value.as_sequence()?, name);
        }
        value.as_sequence()?.iter().enumerate().find_map(|(p, provider)| {
            let entries = provider.as_mapping()?.get("api-key-entries")?.as_sequence()?;
            weight_error(entries, &format!("openai-compatibility[{p}].api-key-entries"))
        })
    })
}

/// Go `flattenV8`'s v8 key group expansion (`expandV8Groups`), in family order, the
/// group keys' weights included. `root` is the merge-expanded document.
pub fn v8_group_error(root: &Mapping) -> Option<String> {
    let groups = root.get("api-keys").and_then(Value::as_mapping)?;
    for (_, family) in FAMILIES {
        let Some(groups) = groups.get(*family) else {
            continue;
        };
        let Some(groups) = groups.as_sequence() else {
            return Some(format!("api-keys.{family} must be a list"));
        };
        for (index, group) in groups.iter().enumerate() {
            let Some(group) = group.as_mapping() else {
                return Some(format!("api-keys.{family}[{index}] must be a mapping"));
            };
            let Some(keys) = group.get("keys").and_then(Value::as_sequence) else {
                return Some(format!("api-keys.{family}[{index}].keys must be a list"));
            };
            if let Some(e) = weight_error(keys, &format!("api-keys.{family}.keys")) {
                return Some(e);
            }
            if *family == "openai-compatibility" {
                continue;
            }
            let unsupported = group
                .keys()
                .map(|k| k.as_str().unwrap_or_default())
                .find(|field| !["name", "base-url", "keys"].contains(field) && !SHARED.contains(field));
            if let Some(field) = unsupported {
                return Some(format!("api-keys.{family}: unsupported group field {field}"));
            }
            for key in keys {
                let Some(key) = key.as_mapping() else {
                    return Some(format!("api-keys.{family} key must be a mapping"));
                };
                if key.contains_key("base-url") {
                    return Some(format!("api-keys.{family}: base-url belongs to the group"));
                }
            }
        }
    }
    None
}

/// Go `validateWeightSequenceNode`: the first weight that is not an integer node or is
/// above the limit, as `<path>[<i>].weight: <reason>`.
fn weight_error(items: &[Value], path: &str) -> Option<String> {
    items.iter().enumerate().find_map(|(i, item)| {
        let weight = item.as_mapping()?.get("weight")?;
        let reason = match weight.as_i64() {
            None => "weight must be an integer",
            Some(w) if w > 1_000_000 => "weight must not exceed 1000000",
            Some(_) => return None,
        };
        Some(format!("{path}[{i}].weight: {reason}"))
    })
}

pub fn is_v8_layout(root: &Value) -> bool {
    let mut root = root.clone();
    if super::expand_merges(&mut root).is_err() {
        return false;
    }
    let Some(map) = root.as_mapping() else {
        return false;
    };
    const SECTIONS: &[&str] = &[
        "models",
        "config-version",
        "server",
        "management",
        "access",
        "credentials",
        "requests",
        "oauth",
        "upstream",
        "multimedia",
        "observability",
    ];
    if map.keys().any(|k| k.as_str().is_some_and(|k| SECTIONS.contains(&k)))
        || map.get("api-keys").is_some_and(Value::is_mapping)
    {
        return true;
    }
    CLIENT_PATHS
        .iter()
        .chain(SHARED_PATHS)
        .chain(SHARED_STRUCTS)
        .chain(PATHS)
        .any(|(old, current)| {
            (old != current && lookup(map, current).is_some())
                || (old.starts_with("providers.") && lookup(map, old).is_some())
                || current.strip_prefix("routing.").is_some_and(|rest| {
                    let child = rest.split('.').next().unwrap_or(rest);
                    old != current && lookup(map, &format!("routing.{child}")).is_some()
                })
        })
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

    /// A write that re-emits a migrated legacy section keeps the text of a string setting
    /// written as a number (`0x1F`, not 31), and a read still sees its YAML type, as in Go.
    #[test]
    fn writes_keep_string_settings_written_as_numbers() {
        let original = "oauth-model-alias: {codex: [{name: 0x1F, alias: a}]}\nrouting: {strategy: round-robin}\n";
        let mut doc = ConfigDocument::parse(original).unwrap();
        doc.update(&["routing", "strategy"], Value::from("fill-first"), false)
            .unwrap();
        let text = doc.render_preserving(original).unwrap();
        let cfg = crate::config::Config::parse(&text).unwrap();
        assert_eq!(
            cfg.document["oauth"]["model-alias"]["codex"][0]["name"],
            Value::from("0x1F"),
            "{text}"
        );
        let view = ConfigDocument::view(original).unwrap();
        assert_eq!(
            view.value()["oauth"]["model-alias"]["codex"][0]["name"],
            Value::from(31)
        );
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

    /// Go `TestV8MigrationCarriesProviderKeyFootComments` (3be5fa44).
    #[test]
    fn migration_keeps_provider_and_field_key_foot_comments_once() {
        let original = "oauth:\n    providers:\n        xai:\n            inject-x-search: true\n            \
                        # FIELD KEY FOOT\n        # PROVIDER KEY FOOT\n";
        let text = ConfigDocument::parse(original)
            .unwrap()
            .migrated_text(original)
            .unwrap();
        for marker in ["PROVIDER KEY FOOT", "FIELD KEY FOOT"] {
            assert_eq!(text.matches(marker).count(), 1, "{marker}: {text}");
        }
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
        let d = ConfigDocument::parse("request-retry: 4\ndisable-cooling: true\nrouting: {retry: {request-retry: null}, cooldown: {disable-cooling: false}}\ncodex: {disable-codex-cloaking: true, response-steering: false, optimize-multi-agent-v2: true}\nclient: {codex: {optimize-multi-agent-v2: false}}\noauth: {providers: {codex: {response-steering: true}, xai: {}}}\n").unwrap();
        assert!(d.get(&["routing", "retry", "request-retry"]).unwrap().is_null());
        assert_eq!(
            d.get(&["routing", "cooldown", "disable-cooling"]),
            Some(&Value::Bool(false))
        );
        // v8.0.35: shared settings move to `upstream`; a historical v8 spelling wins
        // over the legacy root one.
        assert_eq!(
            d.get(&["upstream", "codex", "disable-codex-cloaking"]),
            Some(&Value::Bool(true))
        );
        assert_eq!(
            d.get(&["upstream", "codex", "response-steering"]),
            Some(&Value::Bool(true))
        );
        assert_eq!(d.get(&["upstream", "xai"]), Some(&Value::Mapping(Mapping::new())));
        assert!(d.get(&["oauth", "providers"]).is_none(), "{:?}", d.value());
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
