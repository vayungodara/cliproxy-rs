//! `host.auth.*` and `host.affinity.lookup` callbacks (internal/pluginhost/
//! auth_callbacks.go, affinity_callbacks.go): plugins read and save credentials and
//! observe session affinity through the host's auth manager.
//!
//! The server provides the manager ([`AuthManager`], Go's `coreauth.Manager` as the
//! plugin host uses it); without one, listing reads the auth directory and lookups
//! report the manager unavailable, as Go's host does before `SetAuthManager`.

use cpa_core::gostr::GoStr;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use bytes::Bytes;
use serde_json::{Map, Value};

use crate::abi;
use crate::api::{
    AFFINITY_BOUND, AFFINITY_UNSUPPORTED, HostAffinityLookupRequest, HostAffinityLookupResponse, HostAuthFileEntry,
    HostAuthGetRuntimeResponse, HostAuthSaveRequest, HostAuthSaveResponse, HostRecentRequestEntry,
};
use crate::client::CallbackError;
use crate::go_struct;
use crate::gojson::{self, GoTime, Metadata, NonNil, RawJson};
use crate::host::Host;

/// A credential as the manager holds it (the Go `coreauth.Auth` fields the plugin
/// host reads).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct HostAuth {
    pub id: String,
    /// Go `Auth.Index` (`EnsureIndex`).
    pub index: String,
    pub provider: String,
    pub file_name: String,
    pub label: String,
    /// Go `Auth.Status`: `active`, `disabled`, `error`, ...
    pub status: String,
    pub status_message: String,
    pub disabled: bool,
    pub unavailable: bool,
    pub attributes: BTreeMap<String, String>,
    pub metadata: Map<String, Value>,
    pub success: i64,
    pub failed: i64,
    /// Go `RecentRequestsSnapshot`: bucket label, successes, failures, oldest first.
    pub recent_requests: Vec<(String, i64, i64)>,
    pub created_at: GoTime,
    pub updated_at: GoTime,
    pub last_refreshed_at: GoTime,
    pub next_retry_after: GoTime,
    /// Go `Auth.AccountInfo()`.
    pub account_type: String,
    pub account: String,
}

/// What `host.auth.save` registers for the file it writes (Go `buildAuthsFromFileData`).
#[derive(Clone)]
pub enum SavedAuths {
    /// No plugin auth provider took the file: its built-in credential.
    BuiltIn(Box<HostAuth>),
    /// The credentials the plugin auth provider for the file's type parsed, the file's
    /// settings applied. Possibly none: the file then has no credential. `persist`: the
    /// one credential's metadata replaces the file (Go `Manager.persist` with its token
    /// storage); otherwise the file keeps the saved bytes.
    Plugin {
        path: PathBuf,
        credentials: Vec<cpa_core::credential::Credential>,
        persist: bool,
        refresh: Vec<SavedRefresh>,
    },
    /// Nothing yet: the saving plugin is parsing this file, and the file's credentials
    /// come from that parse. `data` is what the save writes.
    Written { path: PathBuf, data: Vec<u8> },
}

/// Where a saved plugin auth's refresh deadlines come from (Go `preserveExistingAuthState`
/// in `buildAuthsFromFileData`): the registered auth it replaces (`donor`, Go
/// `findExistingAuthForSave`), whose `LastRefreshedAt` and `NextRefreshAfter` it takes,
/// unset ones included; without one, the `NextRefreshAfter` its plugin parsed. The
/// manager applies them only once it registers the auth.
#[derive(Clone, Debug)]
pub struct SavedRefresh {
    pub id: String,
    pub donor: Option<String>,
    pub next_refresh_after: Option<chrono::DateTime<chrono::Utc>>,
}

/// The server's credential manager as the plugin host uses it.
pub trait AuthManager: Send + Sync {
    /// Go `Manager.List`.
    fn list(&self) -> Vec<HostAuth>;
    /// Go `Manager.GetByID`.
    fn get_by_id(&self, id: &str) -> Option<HostAuth>;
    /// Go `saveAuthFile` from the write on: `write` puts the file (its error is the
    /// callback's), then the manager registers or updates `auths` and removes the
    /// file's other credentials (`upsertAuthRecordsForPath`), persisting them as its
    /// store does. The manager serialises this with its own credential changes.
    fn save(&self, auths: SavedAuths, write: &mut dyn FnMut() -> Result<(), String>) -> Result<(), String>;
    /// Go `Manager.LookupSessionAffinity`: the status (`bound`, `unbound`, `ambiguous`,
    /// `unsupported`) and, when bound, the credential.
    fn lookup_session_affinity(&self, provider: &str, model: &str, session_id: &str) -> (String, Option<HostAuth>);
}

go_struct! {
    pub struct RpcHostAuthGetRequest("pluginhost.rpcHostAuthGetRequest") {
        "auth_index" => auth_index: String,
    }
}

go_struct! {
    pub struct RpcHostAuthListResponse("pluginhost.rpcHostAuthListResponse") {
        "files" => files: NonNil<Vec<HostAuthFileEntry>>,
    }
}

go_struct! {
    pub struct RpcHostAuthGetResponse("pluginhost.rpcHostAuthGetResponse") {
        "auth_index" => auth_index: String,
        "name" omitempty => name: String,
        "path" omitempty => path: String,
        "json" => json: RawJson,
    }
}

fn err(message: impl Into<String>) -> CallbackError {
    CallbackError::new(message)
}

impl Host {
    /// Go `SetAuthManager`.
    pub fn set_auth_manager(&self, manager: Option<Arc<dyn AuthManager>>) {
        let previous = std::mem::replace(
            &mut *self
                .inner
                .callbacks
                .auth
                .write()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
            manager,
        );
        // Dropped after the lock is released: a manager's drop may call back in.
        drop(previous);
    }

    fn auth_manager(&self) -> Option<Arc<dyn AuthManager>> {
        self.inner
            .callbacks
            .auth
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// Go `resolvedAuthDir`: the configured auth dir, cleaned and absolute.
    fn resolved_auth_dir(&self) -> Option<PathBuf> {
        let cfg = self.config()?;
        let dir = cfg.auth_dir.to_string_lossy().trim().to_owned();
        if dir.is_empty() {
            return None;
        }
        Some(absolute(&crate::platform::clean(Path::new(&dir))))
    }

    /// `host.auth.list`.
    pub(crate) fn host_auth_list(&self, raw: &[u8]) -> Result<Bytes, CallbackError> {
        if !trim_space(raw).is_empty() {
            gojson::from_slice::<Metadata>(raw).map_err(|e| err(format!("decode host auth list request: {e}")))?;
        }
        let files = match self.auth_manager() {
            Some(manager) => {
                let mut entries: Vec<HostAuthFileEntry> = manager.list().iter().filter_map(build_entry).collect();
                entries.sort_by_key(|e| e.name.go_lower());
                entries
            }
            None => self.list_auth_files_from_disk()?,
        };
        Ok(abi::ok_envelope(&RpcHostAuthListResponse { files: NonNil(files) }))
    }

    /// Go `listAuthFilesFromDisk`.
    fn list_auth_files_from_disk(&self) -> Result<Vec<HostAuthFileEntry>, CallbackError> {
        let dir = self
            .resolved_auth_dir()
            .ok_or_else(|| err("auth directory is unavailable"))?;
        let read = std::fs::read_dir(&dir)
            .map_err(|e| err(format!("failed to read auth dir: {}", path_error("open", &dir, &e))))?;
        // os.ReadDir sorts by name.
        let mut dirents: Vec<std::fs::DirEntry> = read.filter_map(Result::ok).collect();
        dirents.sort_by_key(std::fs::DirEntry::file_name);
        let mut files = Vec::new();
        for dirent in dirents {
            if dirent.file_type().is_ok_and(|t| t.is_dir()) {
                continue;
            }
            let name = dirent.file_name().to_string_lossy().into_owned();
            if !name.go_lower().ends_with(".json") {
                continue;
            }
            let full = dir.join(&name);
            let mut entry = HostAuthFileEntry {
                name,
                source: "file".into(),
                path: full.to_string_lossy().into_owned(),
                ..Default::default()
            };
            if let Ok(info) = dirent.metadata() {
                entry.size = info.len() as i64;
                entry.mod_time = mod_time(&info);
            }
            if let Ok(data) = std::fs::read(&full)
                && let Ok(metadata) = gojson::from_slice::<Metadata>(&data)
            {
                if let Some(Value::String(provider)) = metadata.get("type") {
                    entry.auth_type = provider.trim().to_owned();
                    entry.provider = entry.auth_type.clone();
                }
                if let Some(Value::String(email)) = metadata.get("email") {
                    entry.email = email.trim().to_owned();
                }
                if let Some(Value::String(project)) = metadata.get("project_id") {
                    entry.project_id = project.trim().to_owned();
                }
                if let Some(priority) = metadata.get("priority").and_then(parse_priority) {
                    entry.priority = priority;
                }
                if let Some(Value::String(note)) = metadata.get("note") {
                    entry.note = note.trim().to_owned();
                }
                if let Some(Value::String(base_url)) = metadata.get("base_url") {
                    entry.base_url = base_url.trim().to_owned();
                }
                if let Some(websockets) = metadata.get("websockets").and_then(parse_bool_value) {
                    entry.websockets = websockets;
                }
                if metadata.get("disabled").and_then(parse_bool_value) == Some(true) {
                    entry.disabled = true;
                    entry.status = "disabled".into();
                } else {
                    entry.status = "active".into();
                }
            }
            files.push(entry);
        }
        files.sort_by_key(|e| e.name.go_lower());
        Ok(files)
    }

    /// Go `authByIndex`.
    fn auth_by_index(&self, index: &str) -> Result<HostAuth, CallbackError> {
        let index = index.trim();
        if index.is_empty() {
            return Err(err("auth_index is required"));
        }
        let manager = self
            .auth_manager()
            .ok_or_else(|| err("core auth manager unavailable"))?;
        manager
            .list()
            .into_iter()
            .find(|auth| auth.index == index)
            .ok_or_else(|| err(format!("auth not found for auth_index {index}")))
    }

    /// `host.auth.get`: the credential file's JSON.
    pub(crate) fn host_auth_get(&self, raw: &[u8]) -> Result<Bytes, CallbackError> {
        let req: RpcHostAuthGetRequest =
            gojson::from_slice(raw).map_err(|e| err(format!("decode host auth get request: {e}")))?;
        let index = req.auth_index.trim();
        if index.is_empty() {
            return Err(err("auth_index is required"));
        }
        let auth = self.auth_by_index(index)?;
        let path = attribute(&auth, "path").trim().to_owned();
        if path.is_empty() {
            return Err(err(format!("auth file path not found for auth_index {index}")));
        }
        let data = std::fs::read(&path).map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                err(format!("auth file not found for auth_index {index}"))
            } else {
                err(format!(
                    "failed to read auth file: {}",
                    read_error(Path::new(&path), &e)
                ))
            }
        })?;
        if trim_space(&data).is_empty() {
            return Err(err(format!("auth file is empty for auth_index {index}")));
        }
        gojson::from_slice::<Metadata>(&data)
            .map_err(|e| err(format!("invalid auth file for auth_index {index}: {e}")))?;
        let name = match auth.file_name.trim() {
            "" => auth.id.trim().to_owned(),
            name => name.to_owned(),
        };
        Ok(abi::ok_envelope(&RpcHostAuthGetResponse {
            auth_index: index.to_owned(),
            name,
            path,
            json: RawJson(Bytes::from(data)),
        }))
    }

    /// `host.auth.get_runtime`: the credential's runtime entry.
    pub(crate) fn host_auth_get_runtime(&self, raw: &[u8]) -> Result<Bytes, CallbackError> {
        let req: RpcHostAuthGetRequest =
            gojson::from_slice(raw).map_err(|e| err(format!("decode host auth get runtime request: {e}")))?;
        let index = req.auth_index.trim();
        if index.is_empty() {
            return Err(err("auth_index is required"));
        }
        let auth = self.auth_by_index(index)?;
        let entry =
            build_entry(&auth).ok_or_else(|| err(format!("auth runtime info not found for auth_index {index}")))?;
        Ok(abi::ok_envelope(&HostAuthGetRuntimeResponse { auth: entry }))
    }

    /// `host.auth.save`: writes the credential file and registers it.
    pub(crate) fn host_auth_save(&self, caller: &crate::callbacks::Caller, raw: &[u8]) -> Result<Bytes, CallbackError> {
        let req: HostAuthSaveRequest =
            gojson::from_slice(raw).map_err(|e| err(format!("decode host auth save request: {e}")))?;
        // validateHostAuthSaveRequest.
        let name = req.name.trim();
        if name.is_empty() || name.contains(['/', '\\']) || (cfg!(windows) && has_volume_name(name)) {
            return Err(err("invalid auth file name"));
        }
        if !name.go_lower().ends_with(".json") {
            return Err(err("auth file name must end with .json"));
        }
        let data = trim_space(&req.json.0).to_vec();
        if data.is_empty() {
            return Err(err("json is required"));
        }
        gojson::from_slice::<Metadata>(&data).map_err(|e| err(format!("invalid auth json: {e}")))?;
        // saveAuthFile.
        let dir = self
            .resolved_auth_dir()
            .ok_or_else(|| err("auth directory is unavailable"))?;
        let dst = absolute(&dir.join(name));
        // Go parses the saved data first, which recurses through a plugin saving the file
        // it is parsing (`auth::SaveParse`).
        let auths = match self.save_parse(&caller.instance, &dst) {
            crate::auth::SaveParse::Parse => self.build_auths_from_file_data(&dst, &data)?,
            // The file is written as is; the parse's answer is published and the watcher
            // parses the saved bytes next (`credentials::note_plugin_auth_write`).
            crate::auth::SaveParse::Nested => SavedAuths::Written {
                path: dst.clone(),
                data: data.clone(),
            },
            crate::auth::SaveParse::Refused => {
                return Err(err(format!(
                    "auth file {name} is being parsed for another save by this plugin"
                )));
            }
        };
        let mut write = || write_auth_file(&dst, &data).map_err(|e| format!("failed to write auth file: {e}"));
        match self.auth_manager() {
            Some(manager) => manager.save(auths, &mut write).map_err(err)?,
            None => write().map_err(err)?,
        }
        Ok(abi::ok_envelope(&HostAuthSaveResponse {
            name: name.to_owned(),
            path: dst.to_string_lossy().into_owned(),
        }))
    }

    /// Go `buildAuthsFromFileData` for a file about to be written: the plugin auth
    /// provider for the file's type is asked first (a failed parse fails the save), then
    /// the built-in credential. Called holding no lock, so the plugin may call back.
    fn build_auths_from_file_data(&self, path: &Path, data: &[u8]) -> Result<SavedAuths, CallbackError> {
        let mut metadata: Map<String, Value> = match gojson::from_slice::<Metadata>(data) {
            Ok(m) => m.into_iter().collect(),
            Err(e) => return Err(err(format!("invalid auth file: {e}"))),
        };
        crate::auth::normalize_credential_metadata(&mut metadata);
        // ValidateAuthWeight on the file's metadata.
        if let Some(weight) = metadata.get("weight") {
            cpa_core::config::credentials::parse_weight(weight).map_err(|e| {
                err(format!(
                    "invalid auth weight: invalid metadata weight: {}",
                    go_weight_error(weight, e)
                ))
            })?;
        }
        let path_text = path.to_string_lossy().into_owned();
        let default_id = match self.auth_id_for_path(&path_text) {
            id if id.is_empty() => path_text.clone(),
            id => id,
        };
        let parsed = self.parse_auths_for(
            crate::api::AuthParseRequest {
                provider: match metadata.get("type") {
                    Some(Value::String(p)) => p.trim().to_owned(),
                    _ => String::new(),
                },
                path: path_text.clone(),
                file_name: default_id.clone(),
                raw_json: Bytes::copy_from_slice(data),
                ..Default::default()
            },
            true,
        );
        if let Some(e) = parsed.error {
            return Err(err(format!("invalid auth file: {e}")));
        }
        if !parsed.handled {
            // The file's credential keeps its ID.
            let id = self
                .existing_auth_for_save(&default_id, &path_text, &default_id)
                .map(|existing| existing.id.trim().to_owned())
                .filter(|id| !id.is_empty())
                .unwrap_or(default_id);
            return self
                .built_in_auth(path, metadata, id)
                .map(|auth| SavedAuths::BuiltIn(Box::new(auth)));
        }
        let single = parsed.auths.len() == 1;
        let file_disabled = metadata.get("disabled").and_then(parse_bool_value) == Some(true);
        let base = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let mut persist = single;
        let mut auths = Vec::with_capacity(parsed.auths.len());
        let mut refresh = Vec::with_capacity(parsed.auths.len());
        for mut auth in parsed.auths {
            validate_parsed_weight(&auth)?;
            // Normalized before the one auth inherits the file's other fields, so the
            // plugin's own fields win in either spelling.
            let mut own: Map<String, Value> = std::mem::take(&mut auth.metadata).into_iter().collect();
            crate::auth::normalize_credential_metadata(&mut own);
            auth.metadata = own.into_iter().collect();
            if single {
                // The one auth inherits the file's other fields and, without storage of
                // its own, the file itself.
                for (key, value) in &metadata {
                    auth.metadata.entry(key.clone()).or_insert_with(|| value.clone());
                }
                if auth.storage_json.trim_ascii().is_empty() {
                    auth.storage_json = Bytes::copy_from_slice(data);
                }
            }
            if auth.file_name.trim().is_empty() {
                auth.file_name = base.clone();
            }
            // ponytail: a credential lists under its file's name (see
            // DIFFERENCES-FROM-GO.md); a save whose auth names another file is refused
            // before the write. Upgrade: keep the parsed file name on the credential.
            if auth.file_name.trim() != base {
                return Err(err(format!(
                    "auth provider returned file name {} for auth file {base}, which is not supported",
                    auth.file_name.trim()
                )));
            }
            // One auth under the file's own ID takes over the credential the file has.
            if single
                && (auth.id.trim().is_empty() || auth.id == default_id)
                && let Some(existing) = self.existing_auth_for_save(&auth.id, &path_text, &default_id)
                && !existing.id.trim().is_empty()
            {
                auth.id = existing.id.trim().to_owned();
            }
            if auth.id.trim().is_empty() {
                auth.id = default_id.clone();
            }
            // Go `findExistingAuthForSave(auth.ID, path, len(out) == 1)`.
            let donor = if single {
                self.existing_auth_for_save(&auth.id, &path_text, &default_id)
            } else {
                self.auth_manager().and_then(|m| m.get_by_id(auth.id.trim()))
            };
            refresh.push(SavedRefresh {
                id: auth.id.clone(),
                donor: donor.map(|d| d.id),
                next_refresh_after: auth.next_refresh_after.0.map(|at| at.to_utc()),
            });
            if file_disabled {
                auth.disabled = true;
                auth.metadata.insert("disabled".into(), true.into());
            }
            if single {
                // Go `Manager.persist` skips runtime-only and plugin-virtual auths, and a
                // token storage that cannot build its document fails, leaving the file
                // with the saved bytes; the auth is registered either way.
                let marked = |key: &str| auth.attributes.get(key).is_some_and(|v| v.trim().go_eq_fold("true"));
                persist = !marked("runtime_only") && !marked("plugin_virtual");
                if persist {
                    // Go `FileTokenStore.Save` records the disabled state before the
                    // storage serializes, so it stays even when serializing fails.
                    auth.metadata.insert("disabled".into(), auth.disabled.into());
                    if let Err(error) = auth.storage_payload() {
                        tracing::warn!(id = %auth.id, provider = %auth.provider, %error, "failed to persist registered auth");
                        persist = false;
                    }
                }
            }
            auths.push(auth.into_parsed());
        }
        // The saved file's settings Go applies when saving (priority, weight, prefix,
        // proxy URL, ...), from its decoded metadata.
        let credentials =
            cpa_core::config::credentials::plugin_save_credentials(path, &metadata, auths).map_err(err)?;
        Ok(SavedAuths::Plugin {
            path: path.to_owned(),
            credentials,
            persist,
            refresh,
        })
    }

    /// Go `findExistingAuthForSave(authID, path, true)`: the credential with `id` unless
    /// that is the file's default ID, else a non-virtual credential of the file at
    /// `path` (preferring one not under the default ID), else the one with `id`.
    fn existing_auth_for_save(&self, id: &str, path: &str, default_id: &str) -> Option<HostAuth> {
        let manager = self.auth_manager()?;
        let id = id.trim();
        if !id.is_empty()
            && id != default_id
            && let Some(existing) = manager.get_by_id(id)
        {
            return Some(existing);
        }
        let path = absolute(&crate::platform::clean(Path::new(path)));
        let same_file = |auth: &HostAuth| {
            ["path", "source", "virtual_source"].iter().any(|key| {
                let other = attribute(auth, key).trim();
                !other.is_empty() && absolute(&crate::platform::clean(Path::new(other))) == path
            })
        };
        let mut matched: Option<HostAuth> = None;
        for candidate in manager.list() {
            let marked = |key| attribute(&candidate, key).trim().go_eq_fold("true");
            if marked("runtime_only") || marked("plugin_virtual") || !same_file(&candidate) {
                continue;
            }
            if matched
                .as_ref()
                .is_none_or(|m| m.id == default_id && candidate.id != default_id)
            {
                matched = Some(candidate);
            }
        }
        matched.or_else(|| (!id.is_empty()).then(|| manager.get_by_id(id)).flatten())
    }

    /// The built-in branch of Go `buildAuthsFromFileData`.
    fn built_in_auth(&self, path: &Path, metadata: Map<String, Value>, id: String) -> Result<HostAuth, CallbackError> {
        let provider = match metadata.get("type") {
            Some(Value::String(p)) if !p.trim().is_empty() => p.clone(),
            _ => "unknown".to_owned(),
        };
        let label = match metadata.get("email") {
            Some(Value::String(email)) if !email.trim().is_empty() => email.trim().to_owned(),
            _ => provider.clone(),
        };
        let path_text = path.to_string_lossy().into_owned();
        let disabled = metadata.get("disabled").and_then(parse_bool_value) == Some(true);
        let now = GoTime::now_utc();
        let mut auth = HostAuth {
            id,
            provider,
            file_name: path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default(),
            label,
            status: if disabled { "disabled" } else { "active" }.into(),
            disabled,
            attributes: BTreeMap::from([("path".to_owned(), path_text.clone()), ("source".to_owned(), path_text)]),
            metadata,
            created_at: now,
            updated_at: now,
            ..Default::default()
        };
        if let Some(existing) = self.auth_manager().and_then(|m| m.get_by_id(&auth.id)) {
            auth.created_at = existing.created_at;
            auth.last_refreshed_at = existing.last_refreshed_at;
            auth.next_retry_after = existing.next_retry_after;
        }
        // ponytail: ApplyCustomHeadersFromMetadata is left to the manager, which loads
        // the saved file the way every auth file is loaded.
        Ok(auth)
    }

    /// Go `authIDForPath` (auth_callbacks.go): relative to the auth dir when `Rel`
    /// succeeds, otherwise the cleaned absolute path.
    fn auth_id_for_path(&self, path: &str) -> String {
        let path = path.trim();
        if path.is_empty() {
            return String::new();
        }
        let path = absolute(&crate::platform::clean(Path::new(path)))
            .to_string_lossy()
            .into_owned();
        let mut id = path.clone();
        if let Some(dir) = self.resolved_auth_dir()
            && let Some(rel) = crate::platform::rel(&dir.to_string_lossy(), &path)
            && !rel.is_empty()
        {
            id = rel;
        }
        if cfg!(windows) { id.go_lower() } else { id }
    }

    /// `host.affinity.lookup`.
    pub(crate) fn host_affinity_lookup(&self, raw: &[u8]) -> Result<Bytes, CallbackError> {
        let req: HostAffinityLookupRequest =
            gojson::from_slice(raw).map_err(|e| err(format!("decode host affinity lookup request: {e}")))?;
        let (provider, model, session) = (req.provider.trim(), req.model.trim(), req.session_id.trim());
        if provider.is_empty() {
            return Err(err("provider is required"));
        }
        if model.is_empty() {
            return Err(err("model is required"));
        }
        if session.is_empty() {
            return Err(err("session_id is required"));
        }
        let observed_at = GoTime::now_utc();
        let Some(manager) = self.auth_manager() else {
            return Ok(abi::ok_envelope(&HostAffinityLookupResponse {
                status: AFFINITY_UNSUPPORTED.into(),
                observed_at,
                ..Default::default()
            }));
        };
        let resp = match manager.lookup_session_affinity(provider, model, session) {
            (status, Some(auth)) if status == AFFINITY_BOUND => HostAffinityLookupResponse {
                status,
                auth_index: auth.index.clone(),
                observed_at,
                disabled: auth.disabled || auth.status == "disabled",
                unavailable: auth.unavailable || auth.status == "error",
            },
            (status, _) => HostAffinityLookupResponse {
                status,
                observed_at,
                ..Default::default()
            },
        };
        Ok(abi::ok_envelope(&resp))
    }
}

/// Go `buildHostAuthFileEntry`. `None` hides the credential: a disabled runtime-only
/// one, one without a file that is not runtime-only, or a disabled or removed one
/// whose file is gone.
pub fn build_entry(auth: &HostAuth) -> Option<HostAuthFileEntry> {
    let runtime_only = attribute(auth, "runtime_only").trim().eq_ignore_ascii_case("true");
    let disabled = auth.disabled || auth.status == "disabled";
    if runtime_only && disabled {
        return None;
    }
    let path = attribute(auth, "path").trim().to_owned();
    if path.is_empty() && !runtime_only {
        return None;
    }
    let name = match auth.file_name.trim() {
        "" => auth.id.clone(),
        name => name.to_owned(),
    };
    let mut entry = HostAuthFileEntry {
        id: auth.id.clone(),
        auth_index: auth.index.clone(),
        name,
        auth_type: auth.provider.trim().to_owned(),
        provider: auth.provider.trim().to_owned(),
        label: auth.label.clone(),
        status: auth.status.clone(),
        status_message: auth.status_message.clone(),
        disabled: auth.disabled,
        unavailable: auth.unavailable,
        runtime_only,
        source: "memory".into(),
        success: auth.success,
        failed: auth.failed,
        recent_requests: auth
            .recent_requests
            .iter()
            .map(|(time, success, failed)| HostRecentRequestEntry {
                time: time.clone(),
                success: *success,
                failed: *failed,
            })
            .collect(),
        email: auth_email(auth),
        project_id: auth_project_id(auth),
        account_type: auth.account_type.clone(),
        account: auth.account.clone(),
        ..Default::default()
    };
    if !auth.created_at.is_zero() {
        entry.created_at = auth.created_at;
    }
    if !auth.updated_at.is_zero() {
        entry.mod_time = auth.updated_at;
        entry.updated_at = auth.updated_at;
    }
    if !auth.last_refreshed_at.is_zero() {
        entry.last_refresh = auth.last_refreshed_at;
    }
    if !auth.next_retry_after.is_zero() {
        entry.next_retry_after = auth.next_retry_after;
    }
    if !path.is_empty() {
        entry.path = path.clone();
        entry.source = "file".into();
        match std::fs::metadata(&path) {
            Ok(info) => {
                entry.size = info.len() as i64;
                entry.mod_time = mod_time(&info);
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                let removed = auth
                    .status_message
                    .trim()
                    .eq_ignore_ascii_case("removed via management api");
                if !runtime_only && (disabled || removed) {
                    return None;
                }
                entry.source = "memory".into();
            }
            Err(_) => {}
        }
    }
    match attribute(auth, "priority").trim() {
        "" => {
            if let Some(priority) = auth.metadata.get("priority").and_then(parse_priority) {
                entry.priority = priority;
            }
        }
        p => {
            if let Ok(parsed) = parse_go_int(p) {
                entry.priority = parsed;
            }
        }
    }
    match attribute(auth, "note").trim() {
        "" => {
            if let Some(Value::String(note)) = auth.metadata.get("note") {
                entry.note = note.trim().to_owned();
            }
        }
        note => entry.note = note.to_owned(),
    }
    match attribute(auth, "base_url").trim() {
        "" => {
            if let Some(Value::String(base_url)) = auth.metadata.get("base_url") {
                entry.base_url = base_url.trim().to_owned();
            }
        }
        base_url => entry.base_url = base_url.to_owned(),
    }
    if let Some(websockets) = auth_websockets(auth) {
        entry.websockets = websockets;
    }
    Some(entry)
}

fn attribute<'a>(auth: &'a HostAuth, key: &str) -> &'a str {
    auth.attributes.get(key).map_or("", String::as_str)
}

/// Go `authEmail`: metadata, then the `email` and `account_email` attributes.
fn auth_email(auth: &HostAuth) -> String {
    if let Some(Value::String(email)) = auth.metadata.get("email") {
        return email.trim().to_owned();
    }
    for key in ["email", "account_email"] {
        let v = attribute(auth, key).trim();
        if !v.is_empty() {
            return v.to_owned();
        }
    }
    String::new()
}

/// Go `authProjectID`.
fn auth_project_id(auth: &HostAuth) -> String {
    if let Some(Value::String(project)) = auth.metadata.get("project_id")
        && !project.trim().is_empty()
    {
        return project.trim().to_owned();
    }
    attribute(auth, "project_id").trim().to_owned()
}

/// Go `authWebsocketsValue`.
fn auth_websockets(auth: &HostAuth) -> Option<bool> {
    let raw = attribute(auth, "websockets").trim();
    if !raw.is_empty()
        && let Some(parsed) = parse_go_bool(raw)
    {
        return Some(parsed);
    }
    auth.metadata.get("websockets").and_then(parse_bool_value)
}

/// Go `parsePriorityValue` on decoded JSON: numbers truncate, strings `Atoi`.
fn parse_priority(raw: &Value) -> Option<i64> {
    match raw {
        Value::Number(n) => n.as_i64().or_else(|| n.as_f64().map(|f| f as i64)),
        Value::String(s) => parse_go_int(s.trim()).ok(),
        _ => None,
    }
}

/// Go `parseBoolValue`.
fn parse_bool_value(raw: &Value) -> Option<bool> {
    match raw {
        Value::Bool(b) => Some(*b),
        Value::String(s) => parse_go_bool(s.trim()),
        _ => None,
    }
}

/// `strconv.ParseBool`.
fn parse_go_bool(s: &str) -> Option<bool> {
    match s {
        "1" | "t" | "T" | "TRUE" | "true" | "True" => Some(true),
        "0" | "f" | "F" | "FALSE" | "false" | "False" => Some(false),
        _ => None,
    }
}

/// `strconv.Atoi`: optional sign and decimal digits, in range.
fn parse_go_int(s: &str) -> Result<i64, ()> {
    let digits = s.strip_prefix(['+', '-']).unwrap_or(s);
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return Err(());
    }
    s.parse().map_err(|_| ())
}

/// Go `strings.TrimSpace` on bytes.
fn trim_space(raw: &[u8]) -> &[u8] {
    match std::str::from_utf8(raw) {
        Ok(s) => s.trim().as_bytes(),
        Err(_) => raw.trim_ascii(),
    }
}

/// `filepath.Abs` without touching the file system.
fn absolute(path: &Path) -> PathBuf {
    if path.is_absolute() {
        return path.to_owned();
    }
    std::env::current_dir()
        .map(|cwd| crate::platform::clean(&cwd.join(path)))
        .unwrap_or_else(|_| path.to_owned())
}

/// Go `ValidateAuthWeight` on an auth a plugin parsed.
fn validate_parsed_weight(auth: &crate::auth::PluginAuth) -> Result<(), CallbackError> {
    let invalid = |kind: &str, weight: &Value, e: String| {
        err(format!(
            "invalid auth weight: invalid {kind} weight: {}",
            go_weight_error(weight, e)
        ))
    };
    if let Some(weight) = auth.attributes.get("weight") {
        let weight = Value::String(weight.clone());
        cpa_core::config::credentials::parse_weight(&weight).map_err(|e| invalid("attributes", &weight, e))?;
    }
    if let Some(weight) = auth.metadata.get("weight") {
        cpa_core::config::credentials::parse_weight(weight).map_err(|e| invalid("metadata", weight, e))?;
    }
    Ok(())
}

/// Windows `filepath.VolumeName` is non-empty: a drive letter or a UNC prefix.
fn has_volume_name(name: &str) -> bool {
    let b = name.as_bytes();
    (b.len() >= 2 && b[1] == b':' && b[0].is_ascii_alphabetic()) || name.starts_with("\\\\")
}

/// `info.ModTime()` in local time.
fn mod_time(info: &std::fs::Metadata) -> GoTime {
    GoTime(
        info.modified()
            .ok()
            .map(|t| chrono::DateTime::<chrono::Local>::from(t).fixed_offset()),
    )
}

/// `os.WriteFile(path, data, 0o600)`: the error is Go's `*PathError` text for the
/// step that failed (open, write, then close).
pub fn write_auth_file(path: &Path, data: &[u8]) -> Result<(), String> {
    use std::io::Write as _;
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    let mut file = options.open(path).map_err(|e| path_error("open", path, &e))?;
    let written = file.write_all(data).map_err(|e| path_error("write", path, &e));
    let closed = close(file).map_err(|e| path_error("close", path, &e));
    written.and(closed)
}

/// Closes `file`, reporting the error `close(2)` returns (Go's `File.Close`).
fn close(file: std::fs::File) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::fd::IntoRawFd as _;
        // SAFETY: the descriptor is owned here and closed exactly once.
        if unsafe { libc::close(file.into_raw_fd()) } != 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(())
    }
    #[cfg(not(unix))]
    {
        drop(file);
        Ok(())
    }
}

/// `credentialweight.ParseString`'s error for a string weight `strconv.ParseInt`
/// rejects; any other weight error as reported.
// ponytail: cpa-core's parse_weight words string failures with Rust's parse error.
pub(crate) fn go_weight_error(weight: &Value, error: String) -> String {
    let Value::String(raw) = weight else {
        return error;
    };
    let raw = raw.trim();
    match raw.parse::<i64>() {
        Ok(_) => error,
        Err(e) => {
            let why = match e.kind() {
                std::num::IntErrorKind::PosOverflow | std::num::IntErrorKind::NegOverflow => "value out of range",
                _ => "invalid syntax",
            };
            format!(
                "weight must be an integer: strconv.ParseInt: parsing {}: {why}",
                cpa_common::gostr::quote(raw)
            )
        }
    }
}

/// Go's `*PathError` text: `op path: errno`.
pub(crate) fn path_error(op: &str, path: &Path, e: &std::io::Error) -> String {
    format!("{op} {}: {}", path.display(), errno_text(e))
}

/// The error `os.ReadFile` reports: opening fails as `open`, reading a directory as
/// `read`.
fn read_error(path: &Path, e: &std::io::Error) -> String {
    #[cfg(unix)]
    if e.raw_os_error() == Some(libc::EISDIR) {
        return path_error("read", path, e);
    }
    path_error("open", path, e)
}

/// Go's `syscall.Errno` text: strerror in lower case, without Rust's suffix.
fn errno_text(e: &std::io::Error) -> String {
    let text = e.to_string();
    let text = text.split(" (os error").next().unwrap_or_default();
    let mut chars = text.chars();
    chars
        .next()
        .map_or_else(String::new, |c| c.to_lowercase().chain(chars).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn go_number_and_bool_parsing() {
        assert_eq!(parse_go_int("+7"), Ok(7));
        assert_eq!(parse_go_int("-3"), Ok(-3));
        assert_eq!(parse_go_int("7.0"), Err(()));
        assert_eq!(parse_go_int(""), Err(()));
        assert_eq!(parse_priority(&serde_json::json!(4.9)), Some(4));
        assert_eq!(parse_priority(&serde_json::json!(" 5 ")), Some(5));
        assert_eq!(parse_priority(&serde_json::json!(true)), None);
        assert_eq!(parse_bool_value(&serde_json::json!(" T ")), Some(true));
        assert_eq!(parse_bool_value(&serde_json::json!("yes")), None);
    }

    /// credentialweight.ParseString: strconv.ParseInt's syntax and range errors.
    #[test]
    fn string_weight_errors_follow_go() {
        let text = |w: Value| {
            let e = cpa_core::config::credentials::parse_weight(&w).unwrap_err();
            go_weight_error(&w, e)
        };
        assert_eq!(
            text(serde_json::json!(" abc ")),
            "weight must be an integer: strconv.ParseInt: parsing \"abc\": invalid syntax"
        );
        assert_eq!(
            text(serde_json::json!("99999999999999999999")),
            "weight must be an integer: strconv.ParseInt: parsing \"99999999999999999999\": value out of range"
        );
        assert_eq!(text(serde_json::json!("2000000")), "weight must not exceed 1000000");
        assert_eq!(text(serde_json::json!(1.5)), "weight must be an integer");
    }
}
