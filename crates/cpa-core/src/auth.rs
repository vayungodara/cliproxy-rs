//! Credential files in `auth-dir`, in the JSON shape CLIProxyAPI writes.
//!
//! Each file is one credential. The `type` field names the provider. All other fields
//! are kept verbatim so a later write can round-trip fields this crate does not know.

use std::path::{Path, PathBuf};

use serde_json::{Map, Value};

#[derive(Debug, Clone)]
pub struct AuthFile {
    /// File name, which CLIProxyAPI also uses as the credential ID.
    pub id: String,
    pub path: PathBuf,
    pub fields: Map<String, Value>,
}

impl AuthFile {
    pub fn provider(&self) -> &str {
        self.str("type").unwrap_or_default()
    }

    pub fn str(&self, key: &str) -> Option<&str> {
        self.fields.get(key).and_then(Value::as_str)
    }
}

/// Reads every `*.json` credential in `dir`. Unreadable or malformed files are skipped
/// with a warning so one bad file cannot take the proxy down. A missing directory is empty.
pub fn load_dir(dir: &Path) -> anyhow::Result<Vec<AuthFile>> {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e.into()),
    };
    let mut files = Vec::new();
    for entry in entries {
        let path = entry?.path();
        if path.extension().is_none_or(|ext| ext != "json") {
            continue;
        }
        let parsed = std::fs::read(&path)
            .map_err(anyhow::Error::from)
            .and_then(|bytes| Ok(serde_json::from_slice::<Map<String, Value>>(&bytes)?));
        match parsed {
            Ok(fields) => files.push(AuthFile {
                id: path.file_name().unwrap_or_default().to_string_lossy().into_owned(),
                path,
                fields,
            }),
            Err(e) => tracing::warn!(path = %path.display(), error = %e, "skipping credential file"),
        }
    }
    files.sort_by(|a, b| a.id.cmp(&b.id));
    Ok(files)
}

/// A Claude subscription credential (`"type": "claude"`).
#[derive(Debug, Clone)]
pub struct ClaudeCredential {
    pub id: String,
    pub email: String,
    pub access_token: String,
    pub refresh_token: String,
    /// RFC 3339 expiry as written by CLIProxyAPI (`expired`).
    pub expired: String,
}

impl ClaudeCredential {
    pub fn from_file(file: &AuthFile) -> Option<Self> {
        if file.provider() != "claude" {
            return None;
        }
        let access_token = file.str("access_token").filter(|t| !t.is_empty())?;
        Some(Self {
            id: file.id.clone(),
            email: file.str("email").unwrap_or_default().to_owned(),
            access_token: access_token.to_owned(),
            refresh_token: file.str("refresh_token").unwrap_or_default().to_owned(),
            expired: file.str("expired").unwrap_or_default().to_owned(),
        })
    }
}
