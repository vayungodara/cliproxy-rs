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

pub mod credentials;
mod document;
pub mod go_url;
mod sanitize;
mod schema;
mod text;
mod trusted;
mod validate;
pub use document::{ConfigDocument, archive_comments};
pub use schema::validate as validate_config_fields;
pub use schema::{coerce_typed_scalars, go_bool, go_int, written_bool_spelling};

/// Go `expandConfigAliases` plus yaml.v3's merge rules: `<<` keys expand bottom-up
/// (nested merges first); explicit keys win, then earlier merged mappings. A merge
/// value that is not a mapping or a list of mappings is yaml.v3's decode error.
/// (Aliases are already expanded by the parser.)
pub fn expand_merges(value: &mut Value) -> anyhow::Result<()> {
    const NOT_MAPS: &str = "map merge requires map or sequence of maps as the value";
    match value {
        Value::Mapping(map) => {
            for (_, child) in map.iter_mut() {
                expand_merges(child)?;
            }
            if let Some(merge) = map.shift_remove("<<") {
                let sources: Vec<Mapping> = match merge {
                    Value::Mapping(m) => vec![m],
                    Value::Sequence(items) => items
                        .into_iter()
                        .map(|item| match item {
                            Value::Mapping(m) => Ok(m),
                            _ => Err(anyhow::anyhow!(NOT_MAPS)),
                        })
                        .collect::<anyhow::Result<_>>()?,
                    _ => bail!(NOT_MAPS),
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
pub use trusted::{TrustedProxies, go_trim_space};
pub use validate::parse_duration;

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

/// Writes `hash` over the plaintext `secret-key`: in place when the key is written
/// literally, else (inherited through a merge) the merge-expanded tree, as Go's
/// `SaveConfigPreserveCommentsUpdateNestedScalar` does. ponytail: that fallback
/// re-emits without comments; Go keeps the node comments.
fn persist_secret_hash(path: &Path, text: &str, hash: &str) -> anyhow::Result<()> {
    let mut root: Value = serde_yaml_ng::from_str(text)?;
    expand_merges(&mut root)?;
    let mapping = root.as_mapping().context("config must be a mapping")?;
    let v8 = lookup(mapping, "management.secret-key").is_some();
    let parent = if v8 { "management" } else { "remote-management" };
    let loads_hash = |t: &str| Config::parse(t).is_ok_and(|c| c.management.secret_key == hash);
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
    let updated = match in_place.flatten().filter(|t| loads_hash(t)) {
        Some(t) => t,
        None => {
            root.get_mut(parent)
                .and_then(Value::as_mapping_mut)
                .context("management must be a mapping")?
                .insert(Value::from("secret-key"), Value::from(hash));
            serde_yaml_ng::to_string(&root)?
        }
    };
    anyhow::ensure!(loads_hash(&updated), "secret key not updated");
    ConfigDocument::write(path, &updated)
}

impl Config {
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        let text = std::fs::read_to_string(path)?;
        let mut config = Self::parse(&text)?;
        if !config.management.secret_key.is_empty() && !is_bcrypt(&config.management.secret_key) {
            let hash = bcrypt::hash(&config.management.secret_key, 10)?;
            // Go hashes in memory and persists best-effort (a read-only mount starts).
            let _ = persist_secret_hash(path, &text, &hash);
            config.management.secret_key = hash.clone();
            if let Some(management) = config.document.get_mut("management").and_then(Value::as_mapping_mut) {
                management.insert(Value::from("secret-key"), Value::from(hash));
            }
        }
        Ok(config)
    }

    pub fn parse(text: &str) -> anyhow::Result<Self> {
        let mut parsed = serde_yaml_ng::from_str::<Value>(text)?;
        // Before any migration, as Go expands aliases first.
        expand_merges(&mut parsed)?;
        let root = match parsed {
            Value::Null if text.trim().is_empty() => Mapping::new(),
            Value::Mapping(m) => m,
            _ => bail!("config must be a mapping"),
        };
        validate_shape(&root)?;
        let mut document = ConfigDocument::from_mapping(root.clone())?;
        document.coerce_typed_scalars();
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
            derived: Derived::default(),
        })
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
