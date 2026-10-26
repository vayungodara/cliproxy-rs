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

mod binary;
pub mod credentials;
mod document;
pub mod go_url;
mod sanitize;
mod schema;
mod text;
mod trusted;
mod validate;
pub use binary::{PLAIN as PLAIN_SCALAR, double_quoted as yaml_double_quoted};
pub use document::{ConfigDocument, archive_comments, credential_error, is_v8_layout, v8_group_error, v8_layout_error};
pub use schema::validate as validate_config_fields;
pub use schema::{coerce_typed_scalars, go_bool, go_int};

/// A config text with its tagged scalars as yaml.v3 decodes them (`!!binary` values
/// base64-decoded, aliases of them included), merges not yet expanded. The management
/// document reads config text through here and the loader through [`config_value`], so
/// a view or a save candidate sees the tagged values the loader sees.
pub fn yaml_value(text: &str) -> anyhow::Result<Value> {
    let mut value = serde_yaml_ng::from_str::<Value>(text)?;
    binary::decode(text, &mut value, None, binary::Mode::File)?;
    Ok(value)
}

/// [`yaml_value`] as Go's typed decode reads it into the config: a plain scalar in a Go
/// string setting is its text (`1.50`, `0x1F`, `true`), where serde_yaml_ng reads a
/// number or boolean. The management views keep [`yaml_value`]'s YAML types, as Go's
/// read the file's nodes.
pub fn config_value(text: &str) -> anyhow::Result<Value> {
    let mut value = serde_yaml_ng::from_str::<Value>(text)?;
    binary::decode(text, &mut value, Some(&[]), binary::Mode::File)?;
    Ok(value)
}

/// [`Config::plugin_configs`] of `text`, whose parsed root is `root`.
fn plugin_configs(text: &str, root: &Mapping) -> Value {
    if !lookup(root, "plugins.configs").is_some_and(|c| c.as_mapping().is_some_and(|m| !m.is_empty())) {
        return Value::Null;
    }
    let raw = || {
        let mut value = serde_yaml_ng::from_str::<Value>(text)?;
        binary::decode(text, &mut value, Some(&[]), binary::Mode::Plugins)?;
        expand_merges(&mut value)?;
        anyhow::Ok(value)
    };
    match raw() {
        Ok(v) => v
            .get("plugins")
            .and_then(|p| p.get("configs"))
            .cloned()
            .unwrap_or(Value::Null),
        // The same text loaded, so only a limit this read alone hits (alias copies of
        // plugin scalars) gets here.
        Err(e) => {
            // Management reads re-parse the file: warn once per text.
            static WARNED: std::sync::Mutex<u64> = std::sync::Mutex::new(0);
            let hash = {
                use std::hash::{Hash, Hasher};
                let mut h = std::collections::hash_map::DefaultHasher::new();
                text.hash(&mut h);
                h.finish()
            };
            let mut warned = WARNED.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            if *warned != hash {
                *warned = hash;
                tracing::warn!("plugin configs keep their parsed values: {e:#}");
            }
            Value::Null
        }
    }
}

/// A management write's body, the value at `keys` of the config: Go parses it as YAML
/// nodes and saves their text, so a number or boolean in a string setting whose text the
/// save would not write back (`1.50`, `1e3`) is that text. One the save writes back as
/// it is (`5`, `true`) keeps its YAML type, so the saved file and its views stay Go's.
pub fn body_value(text: &str, keys: &[&str]) -> anyhow::Result<Value> {
    fn keep_written(literal: &mut Value, parsed: Value) {
        match (literal, parsed) {
            (Value::Mapping(l), Value::Mapping(p)) => {
                for ((_, l), (_, p)) in l.iter_mut().zip(p) {
                    keep_written(l, p);
                }
            }
            (Value::Sequence(l), Value::Sequence(p)) => {
                for (l, p) in l.iter_mut().zip(p) {
                    keep_written(l, p);
                }
            }
            (l @ Value::String(_), p @ (Value::Number(_) | Value::Bool(_)))
                if l.as_str() == Some(go_string(&p).as_str()) =>
            {
                *l = p;
            }
            _ => {}
        }
    }
    let parsed = serde_yaml_ng::from_str::<Value>(text)?;
    let mut value = parsed.clone();
    binary::decode(text, &mut value, Some(keys), binary::Mode::Body)?;
    keep_written(&mut value, parsed);
    Ok(value)
}

/// yaml.v3's error for a merge value that is not a mapping or a list of mappings.
const MERGE_NOT_MAPS: &str = "yaml: map merge requires map or sequence of maps as the value";

/// Go `expandConfigAliases` plus yaml.v3's merge rules: `<<` keys expand bottom-up
/// (nested merges first); explicit keys win, then earlier merged mappings. A merge
/// value that is not a mapping or a list of mappings is yaml.v3's decode error, even in
/// a merge source an explicit key or an earlier source shadows, which yaml.v3 never
/// decodes.
/// (Aliases are already expanded by the parser.) The parsed value no longer says whether
/// a key was quoted or a merge operand an alias, so [`yaml_value`] refuses a quoted
/// `<<` key (yaml.v3 reads it as a plain key) and fails an alias of a sequence as a
/// merge operand. A JSON management body, which is not read through it, keeps `<<`
/// literal until the completed candidate is expanded, as Go's save ends up writing it plain.
pub fn expand_merges(value: &mut Value) -> anyhow::Result<()> {
    match value {
        Value::Mapping(map) => {
            for (_, child) in map.iter_mut() {
                expand_merges(child)?;
            }
            if let Some(merge) = map.shift_remove("<<") {
                let sources: Vec<Mapping> = match untagged(merge) {
                    Value::Mapping(m) => vec![m],
                    Value::Sequence(items) => items
                        .into_iter()
                        .map(|item| match untagged(item) {
                            Value::Mapping(m) => Ok(m),
                            _ => Err(anyhow::anyhow!(MERGE_NOT_MAPS)),
                        })
                        .collect::<anyhow::Result<_>>()?,
                    _ => bail!(MERGE_NOT_MAPS),
                };
                for source in sources {
                    for (key, item) in source {
                        map.entry(key).or_insert(item);
                    }
                }
            }
        }
        Value::Sequence(items) => {
            for item in items {
                expand_merges(item)?;
            }
        }
        Value::Tagged(tagged) => expand_merges(&mut tagged.value)?,
        _ => {}
    }
    Ok(())
}

/// A merge operand without its local tags: yaml.v3 merges a tagged mapping by its kind.
fn untagged(value: Value) -> Value {
    match value {
        Value::Tagged(tagged) => untagged(tagged.value),
        other => other,
    }
}
pub use trusted::{TrustedProxies, go_trim_space};
pub use validate::{catalog_sources as validate_catalog_sources, go_is_abs, parse_duration};

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
    /// `server.trusted-proxies`, validated. Go applies it at startup only.
    pub trusted_proxies: Vec<String>,
    pub auth_dir: PathBuf,
    pub routing: RoutingConfig,
    pub management: ManagementConfig,
    /// Presence-preserving v8 view, including fields not yet wired into executors.
    pub document: Value,
    /// v8 `oauth.providers.*` leaf paths present in the source text (Go
    /// `Config.OAuthOnlyFields`, keyed by v8 path rather than legacy name). They
    /// apply to OAuth credentials only; see [`Config::for_api_key`]. Settings written
    /// in the legacy layout stay global, as in Go.
    pub oauth_only: std::collections::BTreeSet<String>,
    /// `plugins.configs` as Go hands each plugin its raw node (pluginhost
    /// `runtimeConfigYAML`): a plain scalar that is not plain text to serde_yaml_ng is its
    /// YAML text tagged [`PLAIN_SCALAR`] (`1.50`, `1e3`, `0x1F`, `007`, `!!float 1`), where
    /// [`Config::document`] holds what serde_yaml_ng reads. Null without plugin configs.
    pub plugin_configs: Value,
    /// Values derived from this snapshot, built on first use ([`Config::derived`]).
    pub derived: Derived,
}

/// A per-snapshot cache of values computed from a [`Config`] (for example parsed payload
/// rules), one per type. Everyone holding the same snapshot shares them. A clone starts
/// empty and the cache never affects equality. Derive from configs that will not be
/// mutated afterwards; the runtime's published snapshots are immutable.
#[derive(Default)]
pub struct Derived(std::sync::Mutex<Vec<std::sync::Arc<dyn std::any::Any + Send + Sync>>>);

impl Clone for Derived {
    fn clone(&self) -> Self {
        Self::default()
    }
}

impl PartialEq for Derived {
    fn eq(&self, _: &Self) -> bool {
        true
    }
}

impl Eq for Derived {}

impl Config {
    /// `models.catalog`, `models.codex-catalog` and `models.devin-catalog` (Go
    /// `CatalogSources`); empty when unset.
    pub fn catalog_sources(&self) -> [String; 3] {
        ["catalog", "codex-catalog", "devin-catalog"].map(|name| {
            self.document
                .get("models")
                .and_then(|models| models.get(name))
                .filter(|v| !v.is_null())
                .map(go_string)
                .unwrap_or_default()
        })
    }

    /// The value of type `T` derived from this snapshot, computing it with `init` on first
    /// use. Concurrent first uses may both compute; the first stored value wins.
    pub fn derived<T: std::any::Any + Send + Sync>(&self, init: impl FnOnce(&Config) -> T) -> std::sync::Arc<T> {
        let find = |slots: &[std::sync::Arc<dyn std::any::Any + Send + Sync>]| {
            slots.iter().find_map(|v| v.clone().downcast::<T>().ok())
        };
        let lock = || self.derived.0.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(found) = find(&lock()) {
            return found;
        }
        let value = std::sync::Arc::new(init(self));
        let mut slots = lock();
        if let Some(found) = find(&slots) {
            return found;
        }
        slots.push(value.clone());
        value
    }
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

/// yaml.v3 decodes any scalar into a Go `string` field; serde would reject numbers.
fn de_go_string<'de, D: serde::Deserializer<'de>>(d: D) -> Result<String, D::Error> {
    Ok(go_string(&Value::deserialize(d)?))
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(default, rename_all = "kebab-case")]
pub struct RoutingConfig {
    #[serde(deserialize_with = "de_go_string")]
    pub strategy: String,
    pub force_model_prefix: bool,
    pub session_affinity: bool,
    #[serde(deserialize_with = "de_go_string")]
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
    /// cliproxy-rs only: the longest quota reset trusted at first, as a duration
    /// (`"1h"`) or seconds; empty means one hour, `0` trusts every reset (Go).
    #[serde(deserialize_with = "de_go_string", skip_serializing_if = "String::is_empty")]
    pub max_trusted_cooldown: String,
}

// Do not derive Debug: the key must never appear in diagnostic output.
#[derive(Clone, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(default, rename_all = "kebab-case")]
pub struct ManagementConfig {
    pub allow_remote: bool,
    #[serde(deserialize_with = "de_go_string")]
    pub secret_key: String,
    pub disable_control_panel: bool,
    pub disable_auto_update_panel: bool,
    #[serde(deserialize_with = "de_go_string")]
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

/// Writes `hash` over the plaintext `secret-key` (see [`with_secret_hash`]).
fn persist_secret_hash(path: &Path, text: &str, hash: &str) -> anyhow::Result<()> {
    ConfigDocument::write(path, &with_secret_hash(text, hash)?)
}

/// `text` with `hash` over the effective `secret-key`: in place when the key is written
/// literally and nothing else shares it, else the merge- and alias-expanded tree, as
/// Go's `updateNestedScalarYAML` writes it. ponytail: that fallback re-emits without
/// comments; Go keeps the node comments.
fn with_secret_hash(text: &str, hash: &str) -> anyhow::Result<String> {
    let expanded = |t: &str| -> anyhow::Result<Value> {
        let mut root: Value = serde_yaml_ng::from_str(t)?;
        expand_merges(&mut root)?;
        Ok(root)
    };
    let mut want = expanded(text)?;
    let mapping = want.as_mapping().context("config must be a mapping")?;
    let v8 = lookup(mapping, "management.secret-key").is_some();
    let parent = if v8 { "management" } else { "remote-management" };
    // In place only over a literal key: an inherited one would stay in the merge.
    let literal = serde_yaml_ng::from_str::<Value>(text)
        .ok()
        .and_then(|raw| raw.get(parent)?.as_mapping()?.get("secret-key").cloned())
        .is_some();
    let in_place = literal.then(|| {
        let file: yaml_edit::YamlFile = text.parse().ok()?;
        file.document()?.get_mapping(parent)?.set("secret-key", hash);
        Some(file.to_string())
    });
    want.get_mut(parent)
        .and_then(Value::as_mapping_mut)
        .context("management must be a mapping")?
        .insert(Value::from("secret-key"), Value::from(hash));
    // An anchored key edited in place would also change every alias of it.
    let updated = match in_place.flatten().filter(|t| expanded(t).is_ok_and(|v| v == want)) {
        Some(t) => t,
        None => {
            let t = serde_yaml_ng::to_string(&want)?;
            // The re-emitted tree writes parsed values (`0x1F` becomes `31`) and drops
            // tags; Go keeps each node's text and tag. Refuse rather than change
            // another setting: on any explicit tag, any key that is not a string, or
            // any other scalar whose text would change.
            fn plain_keys(v: &Value) -> bool {
                match v {
                    Value::Mapping(m) => m.iter().all(|(k, v)| matches!(k, Value::String(_)) && plain_keys(v)),
                    Value::Sequence(items) => items.iter().all(plain_keys),
                    Value::Tagged(_) => false,
                    _ => true,
                }
            }
            let tagged = yaml_edit::lex(text)
                .iter()
                .any(|(kind, _)| *kind == yaml_edit::SyntaxKind::TAG);
            anyhow::ensure!(
                !tagged && plain_keys(&want),
                "rewriting the config file would change the text of another setting"
            );
            let others = |t: &str| -> anyhow::Result<Value> {
                let mut tree = scalar_texts(t)?;
                expand_merges(&mut tree)?;
                if let Some(m) = tree.get_mut(parent).and_then(Value::as_mapping_mut) {
                    m.remove("secret-key");
                }
                Ok(tree)
            };
            anyhow::ensure!(
                others(text)? == others(&t)?,
                "rewriting the config file would change the text of another setting"
            );
            t
        }
    };
    let stored = scalar_texts(&updated).ok().map(|tree| effective_secret(&tree));
    anyhow::ensure!(stored.as_deref() == Some(hash), "secret key not updated");
    Ok(updated)
}

/// Go `PrepareConfigFileForUpload` (f9437700): applies the rewrites of a startup load
/// before the file seeds a remote store, so a plaintext management secret never leaves
/// the machine. The effective plaintext secret is hashed and the legacy fields a v8
/// field overrides are dropped. Fails, leaving the file untouched, when the file does
/// not decode, the secret cannot be hashed, or the effective or a legacy secret value
/// would still be a scalar value elsewhere in it (a YAML anchor or a copy). A failure
/// of the write itself can leave a partial file, as in Go.
///
/// Like Go, this only decodes the file: the validators of a startup load, and its
/// credential weight preflight, do not run, so their messages, which quote setting
/// values, cannot carry a secret into the log.
pub fn prepare_for_upload(path: &Path) -> anyhow::Result<()> {
    let text = String::from_utf8(std::fs::read(path)?).context("failed to parse config file")?;
    let prepared = prepared_for_upload(&text)?;
    if prepared != text {
        ConfigDocument::write(path, &prepared).context("failed to write prepared config file")?;
    }
    Ok(())
}

fn prepared_for_upload(text: &str) -> anyhow::Result<String> {
    decode(text, Decode::Upload).context("failed to parse config file")?;
    // yaml.v3 and libyaml end a line at NEL, LS and PS too; yaml-edit, which places
    // the edits and reads the secret's tag, does not, so a comment would hide what
    // follows it. Refuse rather than misread the file.
    anyhow::ensure!(
        !text.contains(['\u{85}', '\u{2028}', '\u{2029}']),
        "the config file contains a Unicode line separator yaml-edit cannot read"
    );
    let mut tree = scalar_texts(text).context("failed to parse config file")?;
    expand_merges(&mut tree).context("failed to parse config file")?;
    let mut secret = effective_secret(&tree).into_bytes();
    if !secret.is_empty() && binary_secret(text, &tree)? {
        // yaml.v3 decodes a `!!binary` scalar into a string field, any bytes.
        secret = binary::go_base64(&secret)
            .context("failed to parse config file: yaml: !!binary value contains invalid base64 data")?;
    }
    let mut prepared = text.to_owned();
    if !secret.is_empty() && !looks_like_bcrypt(&secret) {
        // bcrypt 0.19's non-truncating hash also refuses exactly 72 bytes; Go does not.
        anyhow::ensure!(
            secret.len() <= 72,
            "failed to hash remote management key: bcrypt: password length exceeds 72 bytes"
        );
        let hash = bcrypt::hash(&secret, 10).context("failed to hash remote management key")?;
        prepared = with_secret_hash(text, &hash).context("failed to update remote management key")?;
    }
    if let Some(cleaned) = ConfigDocument::without_conflicts(&prepared)? {
        prepared = cleaned;
    }
    // Go `plaintextManagementSecrets`: the effective secret and the values of both
    // secret keys, since an overridden legacy key still holds its own.
    let mut plaintexts: Vec<String> = String::from_utf8(secret).into_iter().collect();
    for path in ["remote-management.secret-key", "management.secret-key"] {
        if let Some(Value::String(value)) = tree.as_mapping().and_then(|m| lookup(m, path)) {
            plaintexts.push(value.clone());
        }
    }
    plaintexts.retain(|p| !p.is_empty() && !is_bcrypt(p));
    if !plaintexts.is_empty() {
        let doc = scalar_texts(&prepared).context("failed to parse prepared config")?;
        if plaintexts.iter().any(|p| scalar_exists(&doc, p)) {
            bail!(
                "plaintext remote management key is still present elsewhere in the config file, for example through a YAML anchor or as another setting's value"
            );
        }
    }
    Ok(prepared)
}

/// The management secret as Go's `Config` decodes it from a [`scalar_texts`] tree
/// with merges expanded: `management.secret-key` when present (presence wins, even
/// when null), else `remote-management.secret-key`.
fn effective_secret(tree: &Value) -> String {
    let root = tree.as_mapping();
    let at = |path| root.and_then(|m| lookup(m, path));
    match at("management.secret-key").or_else(|| at("remote-management.secret-key")) {
        Some(Value::String(s)) => s.clone(),
        _ => String::new(),
    }
}

/// Whether the effective `secret-key` of a [`scalar_texts`] tree carries yaml.v3's
/// binary tag, resolved as its scanner does: `!!` expands to `tag:yaml.org,2002:`
/// and `%XX` escapes are decoded, so `!!bin%61ry` and `!<tag:yaml.org,2002:binary>`
/// are binary too, as are the tags that resolve to the short spelling `!!binary`
/// (`!<!!binary>`, `!%21binary`), which yaml.v3 also decodes. Fails closed where that
/// resolution is not followed: a `%TAG` directive beside a tagged secret, a named
/// handle, a malformed escape, or a file the syntax tree cannot read. A secret
/// reached through an alias or merge is not resolved: it is refused when any tag in
/// the file could be binary, since the anchor holding the base64 may be dropped by the
/// layout cleanup.
fn binary_secret(text: &str, tree: &Value) -> anyhow::Result<bool> {
    let v8 = tree
        .as_mapping()
        .and_then(|m| lookup(m, "management.secret-key"))
        .is_some();
    let parent = if v8 { "management" } else { "remote-management" };
    let unresolved = || anyhow::anyhow!("the management key's YAML tag cannot be resolved for upload");
    let file: yaml_edit::YamlFile = text.parse().map_err(|_| unresolved())?;
    let tokens = yaml_edit::lex(text);
    let directives = tokens
        .iter()
        .any(|(kind, token)| *kind == yaml_edit::SyntaxKind::DIRECTIVE && token.starts_with("%TAG"));
    let binary = |raw: &str| -> anyhow::Result<bool> {
        anyhow::ensure!(!directives, unresolved());
        let tag = if let Some(uri) = raw.strip_prefix("!<").and_then(|t| t.strip_suffix('>')) {
            uri.to_owned()
        } else if let Some(suffix) = raw.strip_prefix("!!") {
            format!("tag:yaml.org,2002:{suffix}")
        } else if raw[1..].contains('!') {
            anyhow::bail!(unresolved());
        } else {
            raw.to_owned()
        };
        let mut bytes = Vec::with_capacity(tag.len());
        let mut rest = tag.as_bytes();
        while let Some((&b, tail)) = rest.split_first() {
            if b == b'%' {
                let hex = tail.get(..2).and_then(|h| std::str::from_utf8(h).ok());
                bytes.push(
                    hex.and_then(|h| u8::from_str_radix(h, 16).ok())
                        .ok_or_else(unresolved)?,
                );
                rest = &tail[2..];
            } else {
                bytes.push(b);
                rest = tail;
            }
        }
        Ok(bytes == b"tag:yaml.org,2002:binary" || bytes == b"!!binary")
    };
    match file.document().and_then(|d| d.get_mapping(parent)?.get("secret-key")) {
        Some(yaml_edit::YamlNode::TaggedNode(node)) => binary(&node.tag().ok_or_else(unresolved)?),
        Some(yaml_edit::YamlNode::Scalar(_)) => Ok(false),
        // Through an alias, a merge or an aliased section.
        _ => {
            for (kind, token) in &tokens {
                if *kind == yaml_edit::SyntaxKind::TAG && binary(token).unwrap_or(true) {
                    anyhow::bail!(unresolved());
                }
            }
            Ok(false)
        }
    }
}

/// `text` as a tree whose scalars are their decoded text, as yaml.v3's `Node.Value`
/// holds it (`0x1F` stays `0x1F`, `'31'` is `31`, `!!binary c2VjcmV0` is `c2VjcmV0`),
/// and whose nulls stay null. Custom tags are dropped. Keys are kept as parsed;
/// aliases are expanded, merges are not.
fn scalar_texts(text: &str) -> anyhow::Result<Value> {
    use serde::de::{DeserializeSeed, Deserializer, IgnoredAny, MapAccess, SeqAccess, Visitor};
    struct Shaped<'a>(&'a Value);
    struct Entries<'a>(&'a Mapping);
    struct Items<'a>(&'a [Value]);
    impl<'de> DeserializeSeed<'de> for Shaped<'_> {
        type Value = Value;
        fn deserialize<D: Deserializer<'de>>(self, d: D) -> Result<Value, D::Error> {
            match self.0 {
                Value::Mapping(map) => d.deserialize_map(Entries(map)),
                Value::Sequence(items) => d.deserialize_seq(Items(items)),
                Value::Null => IgnoredAny::deserialize(d).map(|_| Value::Null),
                // A custom tag reaches serde as an enum variant; Go reads the node.
                Value::Tagged(tagged) => d.deserialize_any(Untagged(&tagged.value)),
                _ => String::deserialize(d).map(Value::String),
            }
        }
    }
    struct Untagged<'a>(&'a Value);
    impl<'de> Visitor<'de> for Untagged<'_> {
        type Value = Value;
        fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
            f.write_str("a tagged node")
        }
        fn visit_enum<A: serde::de::EnumAccess<'de>>(self, access: A) -> Result<Value, A::Error> {
            use serde::de::VariantAccess as _;
            let (IgnoredAny, variant) = access.variant()?;
            match self.0 {
                Value::Mapping(_) | Value::Sequence(_) => variant.newtype_variant_seed(Shaped(self.0)),
                // Text even when spelled `null` or `~`: only a `!!null` node is null.
                _ => variant.newtype_variant::<String>().map(Value::String),
            }
        }
    }
    impl<'de> Visitor<'de> for Entries<'_> {
        type Value = Value;
        fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
            f.write_str("a mapping")
        }
        fn visit_map<A: MapAccess<'de>>(self, mut access: A) -> Result<Value, A::Error> {
            let mut out = Mapping::new();
            for (key, value) in self.0 {
                access.next_key::<IgnoredAny>()?;
                out.insert(key.clone(), access.next_value_seed(Shaped(value))?);
            }
            Ok(Value::Mapping(out))
        }
    }
    impl<'de> Visitor<'de> for Items<'_> {
        type Value = Value;
        fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
            f.write_str("a sequence")
        }
        fn visit_seq<A: SeqAccess<'de>>(self, mut access: A) -> Result<Value, A::Error> {
            let mut out = Vec::with_capacity(self.0.len());
            for item in self.0 {
                out.push(access.next_element_seed(Shaped(item))?.unwrap_or(Value::Null));
            }
            Ok(Value::Sequence(out))
        }
    }
    use serde::Deserialize as _;
    let shape: Value = serde_yaml_ng::from_str(text)?;
    Ok(Shaped(&shape).deserialize(serde_yaml_ng::Deserializer::from_str(text))?)
}

/// Go `yamlScalarExists`: a non-null scalar whose text is `value` anywhere below `node`
/// (a [`scalar_texts`] tree). Mapping keys only name settings and are skipped.
fn scalar_exists(node: &Value, value: &str) -> bool {
    match node {
        Value::Mapping(map) => map.values().any(|v| scalar_exists(v, value)),
        Value::Sequence(items) => items.iter().any(|v| scalar_exists(v, value)),
        Value::String(text) => text == value,
        _ => false,
    }
}

/// Which Go entry point [`decode`] follows.
#[derive(Clone, Copy, PartialEq)]
enum Decode {
    /// `LoadConfig`: the credential preflight (`validateCredentialWeightYAML`), then the
    /// typed decode.
    Load,
    /// `ParseConfigBytes`: as `Load`, with typed-decode errors wrapped as
    /// `parse config payload: ...` (Go wraps its `yaml.Unmarshal`).
    Payload,
    /// `prepareConfigForUpload`: the typed decode only, so of the credential checks only
    /// the v8 key groups, which `flattenV8` makes while decoding.
    Upload,
}

/// The decoding part of [`Config::parse`]: the YAML, its layout and the setting types,
/// before the validators. Its errors name settings, not their values. The layout checks
/// come first, unwrapped, as in Go.
fn decode(text: &str, mode: Decode) -> anyhow::Result<(Mapping, ConfigDocument)> {
    let mut parsed = config_value(text)?;
    // Before any migration, as Go expands aliases first.
    expand_merges(&mut parsed)?;
    let root = match parsed {
        Value::Null if text.trim().is_empty() => Mapping::new(),
        Value::Mapping(m) => m,
        _ => bail!("config must be a mapping"),
    };
    validate_shape(&root)?;
    let credentials = if mode == Decode::Upload {
        v8_group_error(&root)
    } else {
        credential_error(&root)
    };
    if let Some(e) = credentials {
        bail!(e);
    }
    let mut document = ConfigDocument::from_mapping(root.clone())?;
    document.coerce_typed_scalars();
    schema::validate(document.value(), false)
        .and_then(|()| validate::decoded(document.value()))
        .map_err(|e| {
            if mode == Decode::Payload {
                anyhow::anyhow!("parse config payload: {e}")
            } else {
                e
            }
        })?;
    Ok((root, document))
}

impl Config {
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        Self::load_text(path, &std::fs::read_to_string(path)?)
    }

    /// [`Config::load`] of text already read from `path`, so a caller knows exactly which
    /// bytes it parsed.
    pub fn load_text(path: &Path, text: &str) -> anyhow::Result<Self> {
        let mut config = Self::parse(text)?;
        if !config.management.secret_key.is_empty() && !is_bcrypt(&config.management.secret_key) {
            let hash = bcrypt::hash(&config.management.secret_key, 10)?;
            // Go hashes in memory and persists best-effort (a read-only mount starts).
            let _ = persist_secret_hash(path, text, &hash);
            config.management.secret_key = hash.clone();
            if let Some(management) = config.document.get_mut("management").and_then(Value::as_mapping_mut) {
                management.insert(Value::from("secret-key"), Value::from(hash));
            }
        }
        Ok(config)
    }

    /// Go `ParseConfigBytes`, as the v8 config writer reports it: [`Config::parse`] with
    /// typed-decode errors wrapped as `parse config payload: ...` (see [`decode`]).
    pub fn parse_payload(text: &str) -> anyhow::Result<Self> {
        Self::parse_as(text, Decode::Payload)
    }

    pub fn parse(text: &str) -> anyhow::Result<Self> {
        Self::parse_as(text, Decode::Load)
    }

    fn parse_as(text: &str, mode: Decode) -> anyhow::Result<Self> {
        let (root, document) = decode(text, mode)?;
        let canonical = document.value().as_mapping().expect("document is a mapping");
        let routing = decode_section::<RoutingConfig>(canonical, "routing")?;
        let mut routing = routing;
        routing.retry.max_retry_credentials = routing.retry.max_retry_credentials.max(0);
        let management = decode_section::<ManagementConfig>(canonical, "management")?;
        let pick = |v8: &str, legacy: &str| lookup(&root, v8).or_else(|| lookup(&root, legacy));
        let host = string_or_empty(pick("server.host", "host"), "server.host")?;
        let port = match pick("server.port", "port") {
            None | Some(Value::Null) => 0,
            // yaml.v3 truncates a float into Go's int port.
            Some(v) => go_int(v)
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
        let trusted_proxies: Vec<String> = match lookup(canonical, "server.trusted-proxies") {
            Some(Value::Sequence(seq)) => seq.iter().map(go_string).collect(),
            _ => Vec::new(),
        };
        trusted::validate(&trusted_proxies)?;
        validate::go_custom(document.value())?;
        let oauth_only = schema::oauth_only_paths(&Value::Mapping(root.clone()));
        let plugin_configs = plugin_configs(text, &root);
        Ok(Config {
            host,
            port,
            api_keys: normalize_keys(api_keys),
            trusted_proxies,
            auth_dir: resolve_auth_dir(&auth_dir),
            routing,
            management,
            document: document.into_value(),
            oauth_only,
            plugin_configs,
            derived: Derived::default(),
        })
    }

    /// `server.max-request-body-bytes` (legacy root `max-request-body-bytes`), Go
    /// `SDKConfig.MaxRequestBodyBytes`: the request body limit in bytes. `None` when
    /// omitted or not positive, which disables it (Go's default 0).
    pub fn max_request_body_bytes(&self) -> Option<u64> {
        let value = self.document.get("server")?.get("max-request-body-bytes")?;
        u64::try_from(go_int(value)?).ok().filter(|&n| n > 0)
    }

    /// Go `Config.ForAPIKey`: the view an API-key credential sees, with the v8
    /// OAuth-only provider settings set to their typed zero. Borrowed when there are
    /// none.
    pub fn for_api_key(&self) -> std::borrow::Cow<'_, Config> {
        if self.oauth_only.is_empty() {
            return std::borrow::Cow::Borrowed(self);
        }
        let mut view = self.clone();
        for path in &self.oauth_only {
            schema::zero_at(&mut view.document, path);
        }
        view.oauth_only.clear();
        std::borrow::Cow::Owned(view)
    }
}

/// Go `looksLikeBcrypt`.
pub fn is_bcrypt(key: &str) -> bool {
    looks_like_bcrypt(key.as_bytes())
}

fn looks_like_bcrypt(key: &[u8]) -> bool {
    key.len() > 4 && [b"$2a$", b"$2b$", b"$2y$"].iter().any(|p| key.starts_with(*p))
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
    if let Some(e) = document::shape_error(root) {
        bail!(e);
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

/// A YAML scalar decoded into a Go `string` field (yaml.v3 accepts numbers/bools).
pub fn go_string(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        Value::Number(n) => n.to_string(),
        Value::Bool(b) => b.to_string(),
        _ => String::new(),
    }
}

fn string_or_empty(value: Option<&Value>, name: &str) -> anyhow::Result<String> {
    match value {
        None | Some(Value::Null) => Ok(String::new()),
        Some(Value::String(s)) => Ok(s.clone()),
        // yaml.v3 decodes any scalar into a Go string field as its text.
        Some(v @ (Value::Number(_) | Value::Bool(_))) => Ok(go_string(v)),
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

    /// Go `TestPrepareConfigFileForUploadReportsSecretItCannotHash` "read-only file";
    /// tests/prepare_upload_go.rs replays the other Go cases.
    #[cfg(unix)]
    #[test]
    fn prepare_for_upload_refuses_a_file_it_cannot_write() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = std::env::temp_dir().join(format!("cpa-prepare-ro-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.yaml");
        let content = "remote-management:\n  secret-key: upload-plaintext-secret\n";
        std::fs::write(&path, content).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o400)).unwrap();
        // Root can write anyway; Go skips there too.
        if std::fs::OpenOptions::new().write(true).open(&path).is_err() {
            assert!(prepare_for_upload(&path).is_err());
            assert_eq!(std::fs::read_to_string(&path).unwrap(), content);
        }
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
    }

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

    /// `worker-threads` is cliproxy-rs only: a whole-file upload through the Management
    /// API keeps it (Go's v8 upload refuses unknown sections), and its type is checked.
    #[test]
    fn worker_threads_is_a_known_int() {
        let doc = |text: &str| serde_yaml_ng::from_str::<serde_yaml_ng::Value>(text).unwrap();
        validate_config_fields(&doc("config-version: 8\nworker-threads: 3\n"), true).unwrap();
        assert!(validate_config_fields(&doc("worker-threads: many\n"), true).is_err());
        assert!(validate_config_fields(&doc("worker-thread: 3\n"), true).is_err());
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
    fn image_generation_mode_reads_go_spellings() {
        let mode = |yaml: &str| {
            Config::parse(&format!("multimedia:\n  disable-image-generation: {yaml}\n"))
                .map(|c| c.document["multimedia"]["disable-image-generation"].clone())
                .map_err(|e| e.to_string())
        };
        assert_eq!(mode("' Off '"), Ok(Value::Bool(false)));
        assert_eq!(mode("~"), Ok(Value::Bool(false)));
        assert_eq!(mode("YES"), Ok(Value::Bool(true)));
        assert_eq!(mode("' Chat'"), Ok(Value::from("chat")));
        assert_eq!(
            mode("\"\\u00dc\\u0001\""),
            Err(
                "invalid disable-image-generation value \"\u{fc}\\x01\" (allowed: true, false, chat, passthrough)"
                    .into()
            )
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
        // cliproxy-rs's own cooldown key: an int or a duration, kept as written, and
        // part of the schema so management writes do not archive it.
        for (yaml, expect) in [("0", "0"), ("90m", "90m")] {
            let text = format!("routing:\n  cooldown:\n    max-trusted-cooldown: {yaml}\n");
            let cfg = Config::parse(&text).unwrap();
            assert_eq!(cfg.routing.cooldown.max_trusted_cooldown, expect);
            assert!(ConfigDocument::parse(&text).unwrap().archive_unknown().is_empty());
        }
        assert!(Config::parse("routing: {cooldown: {max-trusted-cooldown: [1]}}").is_err());
        let empty = Config::default();
        assert_eq!(empty.routing, RoutingConfig::default());
        assert_eq!(empty.routing.session_affinity_subagents, None);
        let null = Config::parse("request-retry: 7\nrouting: {retry: {request-retry: null}}\n").unwrap();
        assert_eq!(null.routing.retry.request_retry, 0);
    }

    /// v8.0.35 `buildV8Paths` moves the legacy `claude.session-date-pin-days` to
    /// `upstream.claude`.
    #[test]
    fn legacy_session_date_pin_days_migrates() {
        let doc = ConfigDocument::parse("claude: {tool-search: true, session-date-pin-days: 3}\n").unwrap();
        assert!(doc.value().get("claude").is_none(), "{:?}", doc.value());
        assert_eq!(doc.value()["upstream"]["claude"]["session-date-pin-days"], 3);
    }

    /// Go `shared_upstream_test.go` (52d5507d): the legacy, canonical `upstream` and
    /// historical `oauth.providers` layouts load the same shared settings for OAuth and
    /// API-key views alike, migrate without their aliases, and keep the Codex header
    /// defaults OAuth-only.
    #[test]
    fn shared_upstream_layouts_agree_and_migrate() {
        let keys = "api-keys: {codex: [{base-url: https://example.invalid, keys: [{api-key: test-key, disable-codex-cloaking: false}]}]}\n";
        let codex = "{disable-codex-cloaking: true, stream-bootstrap-buffering: true, stream-bootstrap-timeout: 10s, orphan-delegation-compatibility: true, model-level-cooling: true, response-steering: true";
        let headers = "{user-agent: client-agent, package-version: 1.2.3, runtime-version: v22.1.0, os: Linux, arch: arm64, timeout: '123', timezone: Asia/Shanghai, stabilize-device-profile: true}";
        let oauth = "oauth: {auth-dir: client-auth, auth-auto-refresh-workers: 3, providers: {codex: {header-defaults: {user-agent: oauth-agent, beta-features: oauth-beta}}}}\n";
        let legacy = format!(
            "auth-dir: client-auth\nauth-auto-refresh-workers: 3\ncodex: {codex}}}\nclaude: {{model-level-cooling: true}}\nclaude-code: {{disable-cloaking-model-list: true}}\ndisable-claude-cloak-mode: true\nclaude-header-defaults: {headers}\nxai: {{inject-x-search: true}}\n{oauth}{keys}"
        );
        let canonical = format!(
            "upstream:\n  codex: {codex}}}\n  claude:\n    model-level-cooling: true\n    disable-cloaking-model-list: true\n    disable-claude-cloak-mode: true\n    header-defaults: {headers}\n  xai: {{inject-x-search: true}}\n{oauth}{keys}"
        );
        let historical = format!(
            "oauth:\n  auth-dir: client-auth\n  auth-auto-refresh-workers: 3\n  providers:\n    codex: {codex}, header-defaults: {{user-agent: oauth-agent, beta-features: oauth-beta}}}}\n    claude:\n      model-level-cooling: true\n      claude-code: {{disable-cloaking-model-list: true}}\n      disable-claude-cloak-mode: true\n      header-defaults: {headers}\n    xai: {{inject-x-search: true}}\n{keys}"
        );
        let expected = Config::parse(&canonical).unwrap().document;
        assert_eq!(
            expected["upstream"]["claude"]["header-defaults"]["timezone"],
            "Asia/Shanghai"
        );
        for raw in [&legacy, &canonical, &historical] {
            let doc = ConfigDocument::parse(raw).unwrap();
            schema::validate(doc.value(), true).unwrap();
            for alias in ["codex", "claude", "claude-code", "xai", "claude-header-defaults"] {
                assert!(doc.value().get(alias).is_none(), "{alias} survived: {raw}");
            }
            let providers = &doc.value()["oauth"]["providers"];
            assert!(
                providers.get("claude").is_none() && providers.get("xai").is_none(),
                "{raw}"
            );
            let codex = providers["codex"].as_mapping().unwrap();
            assert_eq!(
                codex.keys().collect::<Vec<_>>(),
                [&Value::from("header-defaults")],
                "{raw}"
            );
            for text in [raw.to_string(), doc.yaml().unwrap()] {
                let cfg = Config::parse(&text).unwrap();
                for view in [&cfg, &*cfg.for_api_key()] {
                    assert_eq!(view.document["upstream"], expected["upstream"], "{text}");
                    assert_eq!(view.document["oauth"]["auth-auto-refresh-workers"], 3);
                    let key = &view.document["api-keys"]["codex"][0]["keys"][0];
                    assert_eq!(key["disable-codex-cloaking"], Value::Bool(false));
                }
                let user_agent =
                    |c: &Config| c.document["oauth"]["providers"]["codex"]["header-defaults"]["user-agent"].clone();
                assert_eq!(user_agent(&cfg), "oauth-agent");
                assert_eq!(user_agent(&cfg.for_api_key()), "");
            }
        }
    }

    /// Go `TestSharedUpstreamPresenceWinsOverHistoricalAliases` and
    /// `TestSharedUpstreamEmptyHistoricalContainers`.
    #[test]
    fn shared_upstream_presence_wins_and_empty_containers_move() {
        for value in ["false", "null"] {
            let raw = format!(
                "codex: {{response-steering: true}}\noauth: {{providers: {{codex: {{response-steering: true}}, claude: {{header-defaults: {{user-agent: historical-agent, stabilize-device-profile: true}}}}}}}}\nupstream: {{codex: {{response-steering: {value}}}, claude: {{header-defaults: {{user-agent: '', stabilize-device-profile: {value}}}}}}}\n"
            );
            let cfg = Config::parse(&raw).unwrap();
            let upstream = &cfg.document["upstream"];
            assert_ne!(upstream["codex"]["response-steering"], Value::Bool(true), "{raw}");
            assert_eq!(upstream["claude"]["header-defaults"]["user-agent"], "", "{raw}");
            assert_ne!(
                upstream["claude"]["header-defaults"]["stabilize-device-profile"],
                Value::Bool(true)
            );
            schema::validate(ConfigDocument::parse(&raw).unwrap().value(), true).unwrap();
        }
        for (historical, current) in [
            (
                "oauth.providers.claude.header-defaults",
                "upstream.claude.header-defaults",
            ),
            ("oauth.providers.claude.claude-code", "upstream.claude"),
            ("oauth.providers.claude", "upstream.claude"),
            ("oauth.providers.xai", "upstream.xai"),
        ] {
            for value in ["{}", "null"] {
                let parts: Vec<&str> = historical.split('.').collect();
                let raw = parts
                    .iter()
                    .rev()
                    .fold(value.to_owned(), |inner, key| format!("{{{key}: {inner}}}"));
                let doc = ConfigDocument::parse(&raw).unwrap();
                schema::validate(doc.value(), true).unwrap();
                assert!(lookup(doc.value().as_mapping().unwrap(), historical).is_none(), "{raw}");
                assert!(lookup(doc.value().as_mapping().unwrap(), current).is_some(), "{raw}");
            }
        }
        // The legacy Claude structs move by their declared fields only: Go has no
        // `claude.disable-cloaking-model-list` or `claude-code.model-level-cooling`.
        let cfg = Config::parse(
            "claude: {disable-cloaking-model-list: true, model-level-cooling: true}\nclaude-code: {disable-cloaking-model-list: false, model-level-cooling: true}\n",
        )
        .unwrap();
        let claude = &cfg.document["upstream"]["claude"];
        assert_eq!(claude["disable-cloaking-model-list"], Value::Bool(false));
        assert_eq!(claude["model-level-cooling"], Value::Bool(true));
        assert_eq!(cfg.document["claude"]["disable-cloaking-model-list"], Value::Bool(true));
        assert_eq!(cfg.document["claude-code"]["model-level-cooling"], Value::Bool(true));
        let cfg = Config::parse("claude-code: {model-level-cooling: true}\n").unwrap();
        assert!(cfg.document.get("upstream").is_none(), "{:?}", cfg.document);
        for legacy in ["claude: null\n", "claude-code: {}\n"] {
            let doc = ConfigDocument::parse(legacy).unwrap();
            assert_eq!(
                doc.value()["upstream"]["claude"],
                Value::Mapping(Mapping::new()),
                "{legacy}"
            );
            assert!(doc.value().get("claude").is_none() && doc.value().get("claude-code").is_none());
        }
        let err = Config::parse("oauth: {providers: {xai: true}}\n").unwrap_err();
        assert_eq!(err.to_string(), "oauth.providers.xai must be a mapping");
    }

    /// Go `TestIsV8ConfigLayout` (3be5fa44).
    #[test]
    fn v8_layout_is_recognised_by_version_sections_and_paths() {
        for (raw, v8) in [
            ("request-retry: 3\ncodex: {response-steering: true}\n", false),
            (
                "routing: {strategy: fill-first}\nplugins: {configs: {example: {enabled: true}}}\nquota-exceeded: {switch-project: true}\nclient: {codex: {enable-apply-patch: true}}\napi-keys: [client-key]\n",
                false,
            ),
            ("codex: {optimize-multi-agent-v2: true}\n", false),
            ("config-version: 8\nrequest-retry: 3\n", true),
            ("oauth: {providers: {codex: {response-steering: true}}}\n", true),
            ("server: {}\n", true),
            ("upstream: {xai: {inject-x-search: false}}\n", true),
            ("api-keys: {}\n", true),
            ("request-retry: 3\nrouting: {retry: {max-retry-credentials: 2}}\n", true),
            ("routing: {retry: {}}\n", true),
            ("providers: {codex: {optimize-multi-agent-v2: true}}\n", true),
            ("client: {codex: {optimize-multi-agent-v2: false}}\n", true),
        ] {
            let value: Value = serde_yaml_ng::from_str(raw).unwrap();
            assert_eq!(is_v8_layout(&value), v8, "{raw}");
        }
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

    /// Plain numbers that are no model name, alias or exclusion cost one step each: 16
    /// times the entries must cost far less than 256 times the time (a search from the
    /// root per number). Both sizes run in this process, so the ratio does not depend on
    /// the machine.
    #[test]
    fn yaml_literals_scale_linearly() {
        let time = |n: usize| {
            let params: Vec<String> = (0..n).map(|i| format!("k{i}: 1.50")).collect();
            let text = format!(
                "x: &a 1\nrequests: {{payload: {{override: [{{models: [{{name: 0x1F}}], params: {{{}}}}}]}}}}\n",
                params.join(", ")
            );
            (0..3)
                .map(|_| {
                    let start = std::time::Instant::now();
                    let doc = config_value(&text).unwrap();
                    let rule = &doc["requests"]["payload"]["override"][0];
                    assert_eq!(rule["models"][0]["name"], Value::from("0x1F"));
                    assert_eq!(rule["params"][format!("k{}", n - 1).as_str()], Value::from(1.5));
                    start.elapsed()
                })
                .min()
                .unwrap()
        };
        let (small, large) = (time(1000), time(16000));
        let ratio = large.as_secs_f64() / small.as_secs_f64().max(1e-9);
        assert!(
            ratio < 64.0,
            "16x the entries took {ratio:.0}x the time ({small:?} to {large:?})"
        );
    }

    /// The aliases of an anchored list of numbers count toward the alias limit only where
    /// they land in a model name, alias or exclusion. (yaml.v3 refuses this many aliases
    /// as excessive aliasing; the limit here is the copies they make.)
    #[test]
    fn unused_number_aliases_do_not_expand_too_far() {
        let numbers: Vec<String> = (0..1024).map(|i| i.to_string()).collect();
        let text = format!(
            "a: &l [{}]\nb: [{}]\noauth-excluded-models: {{c: *l}}\n",
            numbers.join(", "),
            vec!["*l"; 1024].join(", ")
        );
        let doc = config_value(&text).unwrap();
        assert_eq!(doc["b"][1023][1023], Value::from(1023));
        assert_eq!(doc["oauth-excluded-models"]["c"][7], Value::from("7"));
    }

    /// Aliases whose copies no string setting keeps are bounded too: 2,048 aliases of a
    /// 2,049-number anchor outside any string setting visit just over the limit and are
    /// refused, where each would otherwise copy the whole anchor. Go refuses such a file
    /// as excessive aliasing.
    #[test]
    fn dropped_alias_copies_are_bounded() {
        let numbers = vec!["0"; 2049].join(", ");
        let text = format!("x: &x [{numbers}]\nzz-ignored: [{}]\n", vec!["*x"; 2048].join(", "));
        let err = config_value(&text).unwrap_err();
        assert_eq!(err.to_string(), "config YAML aliases expand too far");
    }

    /// Only alias copies count toward the alias limit: a list of more numbers than that,
    /// anchored (so kept for its aliases) or an exclusion list, loads as in Go.
    #[test]
    fn written_numbers_do_not_count_toward_the_alias_limit() {
        let numbers = vec!["0"; (1 << 20) + 1].join(", ");
        let doc = config_value(&format!(
            "x: &l [{numbers}]
oauth-excluded-models: {{c: [{numbers}]}}
"
        ))
        .unwrap();
        assert_eq!(doc["x"][1 << 20], Value::from(0));
        assert_eq!(doc["oauth-excluded-models"]["c"][1 << 20], Value::from("0"));
    }

    /// yaml.v3 reads a plain scalar into any Go string setting as its text, so a setting
    /// written as a number or boolean (`0x1F`, `1.50`, `007`, `true`) keeps it, in the v8
    /// and legacy layouts and through an anchored key (tests/reference/scalar_settings).
    #[test]
    fn scalar_settings_keep_their_yaml_text() {
        let go: serde_json::Value =
            serde_json::from_str(include_str!("../tests/fixtures/scalar_settings_go.json")).unwrap();
        for case in go["cases"].as_array().unwrap() {
            let cfg = Config::parse(case["yaml"].as_str().unwrap()).unwrap();
            for check in case["checks"].as_array().unwrap() {
                let path = check["rust"].as_str().unwrap();
                let mut node = &cfg.document;
                for part in path.split('.') {
                    node = match part.parse::<usize>() {
                        Ok(i) => &node[i],
                        Err(_) => &node[part],
                    };
                }
                assert_eq!(node.as_str(), check["value"].as_str(), "{}: {path}", case["name"]);
            }
        }
        // A model with a display name written as a number registers under its prefix,
        // both as Go reads them.
        let cfg = Config::parse(go["cases"][0]["yaml"].as_str().unwrap()).unwrap();
        let aliases = crate::registry::dynamic::global_aliases(&cfg);
        let models: Vec<(String, String)> = credentials::from_config(&cfg)
            .iter()
            .flat_map(|c| crate::registry::dynamic::models_for(&cfg, &aliases, c, 0))
            .map(|m| (m.id, m.display_name))
            .collect();
        assert!(models.contains(&("0x1F/a".into(), "1.50".into())), "{models:?}");
    }
}
