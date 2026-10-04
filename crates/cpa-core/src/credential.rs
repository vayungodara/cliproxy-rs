//! Runtime credentials.
//!
//! A credential keeps its complete JSON metadata so fields this crate does not know
//! survive a write-back. Providers read it through their own validated views; this type
//! carries only what routing and persistence need (sdk/cliproxy/auth/types.go).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde_json::{Map, Value};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Source {
    /// A JSON file in `auth-dir`.
    File(PathBuf),
    /// An entry synthesized from `config.yaml` (for example `claude-api-key[i]`).
    Config { section: String, index: usize },
    /// A runtime-only credential with no file or config entry (Go `runtime_only`): an
    /// AI Studio browser connected to the `/v1/ws` relay, for as long as it stays
    /// connected.
    Runtime,
}

#[derive(Debug, Clone)]
pub struct Credential {
    /// Path relative to `auth-dir` for files, as CLIProxyAPI uses.
    pub id: String,
    /// The `type` field: `claude`, `codex`, `antigravity`, ...
    pub provider: String,
    pub source: Source,
    pub disabled: bool,
    /// Email when present, otherwise the provider.
    pub label: String,
    /// Config-derived attributes, never persisted to the credential file.
    pub attributes: BTreeMap<String, String>,
    /// The full credential JSON. Source of truth for provider fields.
    pub metadata: Map<String, Value>,
    /// Store-wide monotonic stamp set on every accepted change, never reused even after
    /// a delete and re-create. Patches against an old revision are rejected.
    pub revision: u64,
}

impl Credential {
    pub fn str(&self, key: &str) -> Option<&str> {
        self.metadata.get(key).and_then(Value::as_str)
    }

    /// The runtime-only `aistudio` credential of a `/v1/ws` relay session (Go
    /// `wsOnConnected`): the channel ID is its ID, label and metadata email.
    pub fn relay_session(channel: &str) -> Self {
        let mut metadata = Map::new();
        metadata.insert("email".into(), Value::String(channel.to_owned()));
        Self {
            id: channel.to_owned(),
            provider: "aistudio".into(),
            source: Source::Runtime,
            disabled: false,
            label: channel.to_owned(),
            attributes: BTreeMap::from([("runtime_only".to_owned(), "true".to_owned())]),
            metadata,
            revision: 0,
        }
    }

    /// Builds a credential from one auth file. Returns `None` for files CLIProxyAPI
    /// ignores here: no `type`, or `gemini-cli` (internal/watcher/synthesizer/file.go).
    pub fn from_file(auth_dir: &Path, path: &Path, metadata: Map<String, Value>) -> Option<Self> {
        let provider = metadata
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        if provider.is_empty() || provider == "gemini-cli" {
            return None;
        }
        let id = path
            .strip_prefix(auth_dir)
            .unwrap_or(path)
            .to_string_lossy()
            .into_owned();
        let mut cred = Self {
            id,
            disabled: false,
            label: String::new(),
            provider,
            source: Source::File(path.to_owned()),
            attributes: BTreeMap::new(),
            metadata,
            revision: 0,
        };
        cred.refresh_derived();
        Some(cred)
    }

    /// Recomputes the fields derived from `metadata`. Call after every metadata change.
    pub fn refresh_derived(&mut self) {
        self.disabled = self.metadata.get("disabled").and_then(Value::as_bool).unwrap_or(false);
        self.label = match self.str("email") {
            Some(email) if !email.is_empty() => email.to_owned(),
            _ => self.provider.clone(),
        };
    }
}

/// A change to credential metadata, produced by token refresh or the management API.
#[derive(Debug, Clone, Default)]
pub struct MetadataPatch {
    pub set: Map<String, Value>,
    pub remove: Vec<String>,
}

impl MetadataPatch {
    pub fn apply(&self, metadata: &mut Map<String, Value>) {
        for key in &self.remove {
            metadata.remove(key);
        }
        for (key, value) in &self.set {
            metadata.insert(key.clone(), value.clone());
        }
    }
}
