//! v8 Sign in with ChatGPT binding (Go v8.0.35 `d50a1995`, `ae3ee3be`:
//! internal/api/handlers/management/oauth_siwc.go, oauth_siwc_mutations.go and
//! sdk/cliproxy/auth/codex_siwc_bind.go). A binding adds the `siwc` token partition and
//! `use_siwc` to an existing file-backed Codex OAuth credential. The session owns the
//! target, PKCE verifier, device ID and redirect; a callback only supplies the code and
//! the issued client ID, through the v8 callback route alone.

use cpa_core::gostr::GoStr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::Response;
use cpa_core::credential::Credential;
use cpa_exec::codex_siwc::{self, REDIRECT_URI, Tokens};
use serde_json::{Map, Value};

use super::Management;
use super::auth_files::{CodexLock, Query, auth_kind, fail, file_name, path_of, reply, runtime_only};
use super::oauth::CallbackRequest;
use crate::runtime::PatchError;

/// Go `siwcBindTTL`.
const TTL: Duration = Duration::from_secs(5 * 60);

/// Go `siwcBindSession`, minus what the session store already tracks.
pub(crate) struct Flow {
    pub(super) target_id: String,
    /// The credential file, symlinks resolved, as checked at start.
    pub(super) path: PathBuf,
    device_id: String,
    /// The file's `account_id` at start: a binding never lands on another account.
    account: Option<String>,
    verifier: String,
    deadline: Instant,
    exchange: Mutex<Option<tokio::task::AbortHandle>>,
}

#[cfg(test)]
impl Flow {
    /// A binding of `target_id` that never exchanges, for session-store tests.
    pub(super) fn idle(target_id: &str) -> Arc<Self> {
        Arc::new(Self {
            target_id: target_id.into(),
            path: PathBuf::new(),
            device_id: String::new(),
            account: None,
            verifier: String::new(),
            deadline: Instant::now() + TTL,
            exchange: Mutex::default(),
        })
    }
}

impl Flow {
    /// Stops an in-flight token exchange. A commit already running finishes; it
    /// re-checks the session before writing.
    pub(super) fn cancel(&self) {
        if let Some(task) = self.exchange.lock().unwrap_or_else(PoisonError::into_inner).take() {
            task.abort();
        }
    }
}

fn non_empty(meta: &Map<String, Value>, key: &str) -> bool {
    meta.get(key)
        .and_then(Value::as_str)
        .is_some_and(|v| !v.trim().is_empty())
}

fn has_outer(meta: &Map<String, Value>) -> bool {
    non_empty(meta, "access_token") && non_empty(meta, "refresh_token")
}

/// Go `siwcBindEligible`.
pub(super) fn eligible(c: &Credential) -> bool {
    c.provider.eq_ignore_ascii_case("codex")
        && path_of(c).is_some()
        && !runtime_only(c)
        && auth_kind(c) == Some("oauth")
        && has_outer(&c.metadata)
}

/// Go `sameSIWCMutationPath`'s canonical form: symlinks resolved, else those of the
/// parent (for a file that is gone), else the absolute path.
fn canonical(path: &Path) -> PathBuf {
    if let Ok(real) = std::fs::canonicalize(path) {
        return real;
    }
    if let (Some(parent), Some(name)) = (path.parent(), path.file_name())
        && let Ok(parent) = std::fs::canonicalize(if parent.as_os_str().is_empty() {
            Path::new(".")
        } else {
            parent
        })
    {
        return parent.join(name);
    }
    std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf())
}

/// The spelling two canonical paths are compared in, never used for I/O: Go's
/// `filepath.Clean` (`.` dropped, `..` applied lexically, never above the root), with
/// the verbatim `\\?\C:\` and `\\?\UNC\server\share` prefixes that
/// `fs::canonicalize` gives on Windows written as `C:\` and `\\server\share`, as
/// Go's `EvalSymlinks` and `Abs` spell them.
///
/// ponytail: a mapped drive that canonicalizes to its UNC share still differs from the
/// drive-letter spelling once its folder is gone; resolving it needs the mount table.
fn clean(path: &Path) -> PathBuf {
    use std::path::{Component, Prefix};
    let mut out = PathBuf::new();
    let mut depth = 0usize;
    for component in path.components() {
        match component {
            Component::Prefix(prefix) => match prefix.kind() {
                Prefix::VerbatimDisk(drive) => out.push(format!("{}:", char::from(drive))),
                Prefix::VerbatimUNC(server, share) => {
                    let mut unc = std::ffi::OsString::from(r"\\");
                    unc.push(server);
                    unc.push(r"\");
                    unc.push(share);
                    out.push(unc);
                }
                _ => out.push(prefix.as_os_str()),
            },
            Component::RootDir => out.push(component),
            Component::CurDir => {}
            Component::ParentDir if depth > 0 => {
                out.pop();
                depth -= 1;
            }
            Component::ParentDir if !out.has_root() => out.push(".."),
            Component::ParentDir => {}
            Component::Normal(name) => {
                out.push(name);
                depth += 1;
            }
        }
    }
    out
}

/// Go `sameSIWCMutationPath`: one file (Go `os.SameFile`, which also catches hard links
/// and another spelling on a case-insensitive file system), else one canonical path,
/// compared case-insensitively on Windows.
pub(super) fn same_path(a: &Path, b: &Path) -> bool {
    if let (Some(x), Some(y)) = (file_id(a), file_id(b))
        && x == y
    {
        return true;
    }
    let (a, b) = (clean(&canonical(a)), clean(&canonical(b)));
    if cfg!(windows) {
        a.to_string_lossy().go_eq_fold(&b.to_string_lossy())
    } else {
        a == b
    }
}

/// The file `path` names after symlinks, as Go's `os.Stat` identifies it.
#[cfg(unix)]
fn file_id(path: &Path) -> Option<(u64, u64)> {
    use std::os::unix::fs::MetadataExt as _;
    std::fs::metadata(path).ok().map(|m| (m.dev(), m.ino()))
}

/// The file `path` names after symlinks, as Go's `os.Stat` identifies it: the volume
/// serial number and file index (`GetFileInformationByHandle`).
#[cfg(windows)]
fn file_id(path: &Path) -> Option<(u64, u64)> {
    use std::os::windows::fs::OpenOptionsExt as _;
    use std::os::windows::io::AsRawHandle as _;
    use windows_sys::Win32::Storage::FileSystem::{
        BY_HANDLE_FILE_INFORMATION, FILE_FLAG_BACKUP_SEMANTICS, GetFileInformationByHandle,
    };
    let file = std::fs::OpenOptions::new()
        .access_mode(0)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
        .open(path)
        .ok()?;
    // SAFETY: BY_HANDLE_FILE_INFORMATION is plain integers and FILETIMEs; all zeros is valid.
    let mut info: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
    // SAFETY: `file` stays open for the call and `info` outlives it.
    let ok = unsafe { GetFileInformationByHandle(file.as_raw_handle(), &mut info) };
    (ok != 0).then(|| {
        (
            u64::from(info.dwVolumeSerialNumber),
            (u64::from(info.nFileIndexHigh) << 32) | u64::from(info.nFileIndexLow),
        )
    })
}

#[cfg(not(any(unix, windows)))]
fn file_id(_: &Path) -> Option<(u64, u64)> {
    None
}

/// Go `resolveSIWCBindFile`: the credential's real path inside `auth-dir` and its
/// current content. The file itself must not be a symlink; the folder may be.
fn resolve(auth_dir: &Path, path: &Path) -> Option<(PathBuf, Map<String, Value>)> {
    let dir = std::fs::canonicalize(auth_dir).ok()?;
    if !std::fs::symlink_metadata(path).ok()?.file_type().is_file() {
        return None;
    }
    let real = std::fs::canonicalize(path).ok()?;
    if real.strip_prefix(&dir).ok()?.as_os_str().is_empty() || !std::fs::metadata(&real).ok()?.is_file() {
        return None;
    }
    let meta: Map<String, Value> = serde_json::from_slice(&std::fs::read(&real).ok()?).ok()?;
    let codex = meta
        .get("type")
        .and_then(Value::as_str)
        .is_some_and(|t| t.trim().eq_ignore_ascii_case("codex"));
    (codex && has_outer(&meta)).then_some((real, meta))
}

/// Go `StartSIWCBind`: `POST /v8/management/oauth/siwc/bind {"name": ...}`.
pub(crate) async fn bind(
    State(state): State<Arc<Management>>,
    crate::body_limit::FirstJson(body): crate::body_limit::FirstJson,
) -> Response {
    // gin `ShouldBindJSON` into `struct{ Name string }`: the first JSON value; members
    // apply in order, the name matched under case folding; null leaves it empty.
    let Some(members) = super::api_call::bind_struct(&body, &["name"]) else {
        return fail(StatusCode::BAD_REQUEST, "invalid request body");
    };
    let mut name = String::new();
    for (_, value) in members {
        match value {
            Value::String(s) => name = s,
            Value::Null => {}
            _ => return fail(StatusCode::BAD_REQUEST, "invalid request body"),
        }
    }
    let name = name.trim().to_owned();
    if name.is_empty() {
        return fail(StatusCode::BAD_REQUEST, "name is required");
    }
    // Go holds its commit gate from the lookup through registration: a v8 upload or
    // delete lands before (and is the target) or after (and fails the binding).
    tokio::task::spawn_blocking(move || {
        let _guard = state.disk.lock().unwrap_or_else(PoisonError::into_inner);
        start(&state, &name)
    })
    .await
    .unwrap_or_else(|_| fail(StatusCode::INTERNAL_SERVER_ERROR, "internal error"))
}

/// [`bind`] after the request is read; callers hold `disk`.
fn start(state: &Management, name: &str) -> Response {
    let all = state.rt.store().snapshot();
    let Some(target) = all
        .iter()
        .find(|c| c.id == name)
        .or_else(|| all.iter().find(|c| file_name(c).as_deref() == Some(name)))
    else {
        return fail(StatusCode::NOT_FOUND, "auth file not found");
    };
    if !eligible(target) {
        return fail(
            StatusCode::BAD_REQUEST,
            "SIWC binding requires a file-backed Codex OAuth credential",
        );
    }
    let cfg = state.rt.config();
    let resolved = path_of(target).and_then(|p| resolve(&cfg.auth_dir, p));
    let Some((path, meta)) = resolved else {
        return fail(StatusCode::BAD_REQUEST, "SIWC binding target is unavailable or invalid");
    };
    // Rebinding keeps a valid device ID.
    let device_id = meta
        .get("siwc")
        .and_then(|s| s.get("device_id"))
        .and_then(Value::as_str)
        .filter(|id| codex_siwc::is_valid_uuid(id))
        .map_or_else(|| uuid::Uuid::new_v4().to_string(), |id| id.trim().go_lower());
    let (Ok((verifier, challenge)), Ok(sid), Ok(nonce)) = (
        cpa_exec::oauth::pkce(),
        codex_siwc::random_base64url(),
        codex_siwc::random_base64url(),
    ) else {
        return fail(
            StatusCode::INTERNAL_SERVER_ERROR,
            "failed to initialize SIWC authorization",
        );
    };
    let identifier = codex_siwc::identifier(&cfg);
    let Ok(url) = codex_siwc::authorize_url(&sid, &nonce, &device_id, REDIRECT_URI, &challenge, &identifier) else {
        return fail(
            StatusCode::INTERNAL_SERVER_ERROR,
            "failed to generate SIWC authorization URL",
        );
    };
    let flow = Arc::new(Flow {
        target_id: target.id.clone(),
        path,
        device_id,
        account: meta
            .get("account_id")
            .and_then(Value::as_str)
            .filter(|a| !a.is_empty())
            .map(str::to_owned),
        verifier,
        deadline: Instant::now() + TTL,
        exchange: Mutex::default(),
    });
    if !state.oauth.register_siwc(&sid, flow, TTL) {
        return fail(
            StatusCode::CONFLICT,
            "a SIWC binding is already pending for this credential",
        );
    }
    reply(
        StatusCode::OK,
        [
            ("status", "ok".into()),
            ("url", url.into()),
            ("state", sid.into()),
            ("flow", "siwc_bind".into()),
            ("expires_in", TTL.as_secs().into()),
        ],
    )
}

fn status_error(status: StatusCode, message: &str) -> Response {
    reply(status, [("status", "error".into()), ("error", message.into())])
}

/// The scheme, host and path of an already-parsed callback URL, as Go compares them,
/// or `None` when it carries user info.
fn callback_target(raw: &str) -> Option<(String, String, String)> {
    // Go `URL.Path`: escapes decoded (Go's parser has rejected bad ones), `+`, `&` and
    // `;` literal, nothing normalized.
    let parsed = cpa_core::config::go_url::parse(raw)?;
    let before_query = raw.split('#').next()?.split('?').next()?;
    let (_, rest) = before_query.split_once("://")?;
    let (authority, path) = rest.find('/').map_or((rest, ""), |i| rest.split_at(i));
    if authority.contains('@') {
        return None;
    }
    let path = super::percent_decode(path);
    let host = parsed.host().to_owned();
    Some((parsed.scheme, host, path))
}

/// Go `handleOAuthCallbackV8` for a binding's state; `None` hands any other state to
/// the ordinary callback. `query` is the GET query, whose keys may not repeat.
pub(super) fn callback(state: &Arc<Management>, req: &CallbackRequest, query: Option<&Query>) -> Option<Response> {
    let mut sid = req.state.trim().to_owned();
    let redirect_raw = req.redirect_url.trim();
    let mut redirect = None;
    if !redirect_raw.is_empty() {
        if cpa_core::config::go_url::parse(redirect_raw).is_none() {
            return Some(status_error(StatusCode::BAD_REQUEST, "invalid redirect_url"));
        }
        let q = Query::parse(Some(
            redirect_raw
                .split('#')
                .next()
                .and_then(|u| u.split_once('?'))
                .map(|(_, q)| q.to_owned())
                .unwrap_or_default(),
        ));
        if sid.is_empty() {
            sid = q.first("state").trim().to_owned();
        }
        redirect = Some(q);
    }
    let flow = state.oauth.siwc(&sid)?;
    let conflicting = || Some(status_error(StatusCode::BAD_REQUEST, "conflicting callback parameters"));
    if let Some(q) = query
        && ["provider", "state", "code", "client_id", "error"]
            .iter()
            .any(|k| q.all(k).len() > 1)
    {
        return conflicting();
    }
    let provider = req.provider.trim();
    if !provider.is_empty() && super::oauth::normalize_callback_provider(provider).as_deref() != Some("codex") {
        return Some(status_error(StatusCode::BAD_REQUEST, "provider does not match state"));
    }
    let (mut code, mut client_id, mut error) = (
        req.code.trim().to_owned(),
        req.client_id.trim().to_owned(),
        req.error.trim().to_owned(),
    );
    if let Some(q) = &redirect {
        let expected = Some((
            "http".to_owned(),
            "127.0.0.1:1455".to_owned(),
            "/auth/callback".to_owned(),
        ));
        let fragment = redirect_raw.split_once('#').is_some_and(|(_, f)| !f.is_empty());
        if fragment || callback_target(redirect_raw) != expected {
            return Some(status_error(StatusCode::BAD_REQUEST, "unexpected SIWC callback URL"));
        }
        let mut given = req.state.trim().to_owned();
        for (key, target) in [
            ("state", &mut given),
            ("code", &mut code),
            ("client_id", &mut client_id),
            ("error", &mut error),
        ] {
            let value = q.first(key).trim();
            if q.all(key).len() > 1 || (!value.is_empty() && !target.is_empty() && value != target) {
                return conflicting();
            }
            if target.is_empty() {
                *target = value.to_owned();
            }
        }
    }
    if error.is_empty() && (code.is_empty() || client_id.is_empty()) {
        return Some(status_error(
            StatusCode::BAD_REQUEST,
            "SIWC callback requires code and client_id",
        ));
    }
    if !state.oauth.begin_siwc(&sid, &flow) {
        return Some(status_error(
            StatusCode::CONFLICT,
            "SIWC session is no longer accepting callbacks",
        ));
    }
    let ok = Some(reply(StatusCode::OK, [("status", "ok".into())]));
    if !error.is_empty() {
        // Never the provider's text: it is readable by anyone holding the state.
        fail_session(state, &sid, &flow, "SIWC authorization was denied or failed");
        return ok;
    }
    let (worker, task_flow) = (state.clone(), flow.clone());
    let task = tokio::spawn(async move {
        let oauth = match &worker.login_base {
            Some(base) => cpa_exec::codex_oauth::CodexOAuth::new(super::oauth::login_client(&worker))
                .with_siwc_token_url(&format!("{base}/api/accounts/oauth/token")),
            None => cpa_exec::codex_oauth::CodexOAuth::new(super::oauth::login_client(&worker)),
        };
        let exchange = oauth.exchange_siwc(
            &code,
            &client_id,
            REDIRECT_URI,
            &task_flow.device_id,
            &task_flow.verifier,
        );
        // Past the deadline the session has expired: nothing to report.
        let Ok(tokens) = tokio::time::timeout_at(task_flow.deadline.into(), exchange).await else {
            return;
        };
        // The credential's preparation lock, which every outer token refresh holds from
        // its read to its commit: the binding lands before or after a refresh, so it
        // never makes a rotated outer token stale (Go keeps both through its mutation
        // gate). Owned and moved into the commit, so a cancel that aborts this task while
        // the commit runs does not release it.
        let lock = worker.rt.store().prepare_lock(&task_flow.target_id);
        let Ok(prepared) = tokio::time::timeout_at(task_flow.deadline.into(), lock.lock_owned()).await else {
            return;
        };
        let tokens = tokens.ok();
        let _ = super::auth_files::with_codex_lock(&worker, move |state, lock| {
            let _prepared = &prepared;
            commit(state, &sid, &task_flow, tokens.as_ref(), lock)
        })
        .await;
    });
    *flow.exchange.lock().unwrap_or_else(PoisonError::into_inner) = Some(task.abort_handle());
    ok
}

/// Go `siwcBindSession.fail`: the fixed message, never an upstream body.
fn fail_session(state: &Management, sid: &str, flow: &Flow, message: &str) {
    state.oauth.set_error(sid, message);
    flow.cancel();
    settled(state, sid);
}

fn settled(state: &Management, sid: &str) {
    if let Some(settled) = &state.login_settled {
        let _ = settled.send(sid.to_owned());
    }
}

/// Go `siwcBindSession.exchange` after the token response. Holds `disk` from the
/// pending check through publication, so v8 uploads and deletes of the file land
/// before (failing the binding) or after it, and the file's credential lock from
/// before the commit is claimed (Go `BindCodexSIWC`'s `WithFileLock`); `Err` names
/// the file when another holder has that lock ([`super::auth_files::with_codex_lock`]).
/// Blocking.
fn commit(
    state: &Management,
    sid: &str,
    flow: &Arc<Flow>,
    tokens: Option<&Tokens>,
    lock: &mut CodexLock,
) -> Result<(), PathBuf> {
    let _guard = state.disk.lock().unwrap_or_else(PoisonError::into_inner);
    if !state.oauth.siwc_is_pending(sid, flow) {
        settled(state, sid);
        return Ok(());
    }
    let Some(tokens) = tokens else {
        fail_session(
            state,
            sid,
            flow,
            "Failed to exchange SIWC authorization code; start a new binding",
        );
        return Ok(());
    };
    let cfg = state.rt.config();
    if resolve(&cfg.auth_dir, &flow.path).is_none_or(|(path, _)| path != flow.path) {
        fail_session(state, sid, flow, "SIWC binding target is no longer available");
        return Ok(());
    }
    let target = state.rt.store().get(&flow.target_id);
    match target
        .as_deref()
        .and_then(path_of)
        .map_or(Ok(()), |path| lock.hold(path))
    {
        Ok(()) => {}
        Err(PatchError::LockBusy(path)) => return Err(path),
        Err(e) => {
            tracing::warn!("SIWC binding was not saved: {e:?}");
            fail_session(
                state,
                sid,
                flow,
                "Failed to save SIWC binding; the target may have changed",
            );
            return Ok(());
        }
    }
    if !state.oauth.claim_siwc_commit(sid, flow) {
        settled(state, sid);
        return Ok(());
    }
    match write_binding(state, flow, tokens, lock) {
        Ok(()) => {
            state.rt.store().recover_unauthorized(&flow.target_id);
            state.oauth.complete(sid);
            settled(state, sid);
        }
        Err(e) => {
            tracing::warn!("SIWC binding was not saved: {e}");
            fail_session(
                state,
                sid,
                flow,
                "Failed to save SIWC binding; the target may have changed",
            );
        }
    }
    Ok(())
}

/// Go `Manager.BindCodexSIWC`: re-reads the current credential and its file and
/// replaces only `siwc` and `use_siwc`, while the credential is still the record it
/// was read from (a refresh in between retries on the refreshed file). A failed write
/// publishes nothing.
///
/// ponytail: Go also rejects a target re-registered since the start (its registration
/// epoch, not reproduced here, M5-0247); this checks the same file and provider and
/// the account the binding started with, and v8 uploads and deletes fail the binding
/// explicitly.
fn write_binding(state: &Management, flow: &Flow, tokens: &Tokens, lock: &mut CodexLock) -> Result<(), String> {
    for _ in 0..8 {
        let current = state
            .rt
            .store()
            .snapshot()
            .into_iter()
            .find(|c| c.id == flow.target_id)
            .ok_or("target is missing")?;
        if !eligible(&current) {
            return Err("target is no longer a file-backed Codex OAuth credential".into());
        }
        let path = path_of(&current).ok_or("target is not file-backed")?;
        // Read, merged and written under the file's credential lock, so a refresh in
        // another process lands before the read or after the write.
        lock.hold(path).map_err(|e| format!("{e:?}"))?;
        if !std::fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_file())
            || std::fs::canonicalize(path).ok().as_deref() != Some(flow.path.as_path())
        {
            return Err("backing file changed".into());
        }
        let disk = std::fs::read(path).map_err(|e| format!("read backing file: {e}"))?;
        let meta: Map<String, Value> = serde_json::from_slice(&disk).map_err(|_| "invalid Codex OAuth file")?;
        let codex = meta
            .get("type")
            .and_then(Value::as_str)
            .is_some_and(|t| t.trim().eq_ignore_ascii_case("codex"));
        if !codex || !has_outer(&meta) {
            return Err("invalid Codex OAuth file".into());
        }
        let disk_account = meta.get("account_id").and_then(Value::as_str);
        let changed = |account: Option<&str>| account.is_some_and(|a| disk_account != Some(a));
        if changed(current.str("account_id").filter(|a| !a.is_empty())) || changed(flow.account.as_deref()) {
            return Err("backing account changed".into());
        }
        let bytes = codex_siwc::bind_file(&disk, tokens).ok_or("invalid Codex OAuth file")?;
        match super::auth_files::save_bytes(state, &current, bytes) {
            Ok(()) => return Ok(()),
            Err(crate::runtime::PatchError::Stale { .. }) => continue,
            Err(e) => return Err(format!("{e:?}")),
        }
    }
    Err("the credential kept changing".into())
}

/// Go's v8 credential-list summary for Codex: support, the toggle, whether a SIWC
/// token set is stored (not whether it works) and its expiry. Never a token.
pub(super) fn summary(c: &Credential, entry: &mut std::collections::BTreeMap<&'static str, Value>) {
    if !c.provider.trim().eq_ignore_ascii_case("codex") {
        return;
    }
    entry.insert("siwc_bind_supported", eligible(c).into());
    // Go `IsCodexUseSIWC`: a true attribute wins, else the file's flag.
    let flag = |v: &Value| match v {
        Value::Bool(b) => Some(*b),
        Value::String(s) => super::auth_files::go_parse_bool(s.trim()),
        Value::Number(n) => n.as_f64().map(|f| f != 0.0),
        _ => None,
    };
    let use_siwc = c
        .attributes
        .get("use_siwc")
        .and_then(|a| super::auth_files::go_parse_bool(a.trim()))
        .filter(|b| *b)
        .or_else(|| c.metadata.get("use_siwc").and_then(flag))
        .unwrap_or(false);
    entry.insert("use_siwc", use_siwc.into());
    let section = c.metadata.get("siwc").and_then(Value::as_object);
    let bound = section
        .is_some_and(|s| non_empty(s, "access_token") && non_empty(s, "refresh_token") && non_empty(s, "client_id"));
    entry.insert("siwc_bound", bound.into());
    if let Some(expiry) = section
        .filter(|s| !s.is_empty())
        .and_then(super::auth_files::siwc_expiry)
    {
        let utc: chrono::DateTime<chrono::Utc> = expiry.into();
        entry.insert(
            "siwc_expires_at",
            utc.to_rfc3339_opts(chrono::SecondsFormat::Secs, true).into(),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::body_limit::FirstJson;

    /// Go `StartSIWCBind` holds its commit gate from the lookup through registration: a
    /// bind waiting on `disk` has read nothing yet, so a replacement made under the lock
    /// (what a v8 upload does) is the target it registers, never the file it replaced.
    #[tokio::test]
    async fn a_bind_reads_its_target_only_under_the_disk_lock() {
        let (dir, file, state) = fixture(None);
        // A writer holds `disk` until told to replace the target and publish it.
        let (locked_tx, locked) = tokio::sync::oneshot::channel();
        let (go, go_rx) = std::sync::mpsc::channel::<()>();
        let writer = tokio::task::spawn_blocking({
            let (state, file, new) = (state.clone(), file.clone(), codex_file("new"));
            move || {
                let _held = state.disk.lock().unwrap();
                locked_tx.send(()).unwrap();
                go_rx.recv().unwrap();
                std::fs::write(&file, new).unwrap();
                state.publish((*state.rt.config()).clone(), None);
            }
        });
        locked.await.unwrap();
        let started = tokio::spawn(bind(
            State(state.clone()),
            FirstJson(axum::body::Bytes::from(r#"{"name":"codex.json"}"#)),
        ));
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(
            !started.is_finished(),
            "the bind waits for disk before reading its target"
        );
        go.send(()).unwrap();
        writer.await.unwrap();
        let response = started.await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            state
                .oauth
                .siwc(&started_state(response).await)
                .unwrap()
                .account
                .as_deref(),
            Some("new")
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// A cancel that aborts the exchange task while its commit runs leaves the commit
    /// holding the preparation lock: an outer refresh cannot read the credential until
    /// the commit returns, so the binding never makes its rotated tokens stale.
    ///
    /// Single-threaded: the exchange task acquires the lock and spawns its commit in one
    /// poll, so once the test sees the lock taken the commit is running (on the blocking
    /// pool, paused on `disk`).
    #[tokio::test]
    async fn a_cancelled_binding_holds_the_preparation_lock_through_its_commit() {
        let app = axum::Router::new().route(
            "/api/accounts/oauth/token",
            axum::routing::post(|| async {
                r#"{"access_token":"siwc-at","refresh_token":"siwc-rt","id_token":"siwc-id","scope":"openid chatgpt.tokens.use.direct","expires_in":3600}"#
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let (dir, _file, state) = fixture(Some(base));
        let response = bind(
            State(state.clone()),
            FirstJson(axum::body::Bytes::from(r#"{"name":"codex.json"}"#)),
        )
        .await;
        let sid = started_state(response).await;
        // Hold `disk` so the commit pauses at its start, holding the preparation lock.
        let (held_tx, held) = std::sync::mpsc::channel();
        let (release, release_rx) = std::sync::mpsc::channel::<()>();
        let holder = std::thread::spawn({
            let state = state.clone();
            move || {
                let _disk = state.disk.lock().unwrap();
                held_tx.send(()).unwrap();
                release_rx.recv().unwrap();
            }
        });
        held.recv().unwrap();
        let req = CallbackRequest {
            state: sid.clone(),
            code: "c".into(),
            client_id: "oaiapp_issued".into(),
            ..Default::default()
        };
        assert_eq!(callback(&state, &req, None).unwrap().status(), StatusCode::OK);
        let lock = state.rt.store().prepare_lock("codex.json");
        tokio::time::timeout(Duration::from_secs(5), async {
            while lock.try_lock().is_ok() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("the commit takes the preparation lock");
        let flow = state.oauth.siwc(&sid).unwrap();
        let task = flow.exchange.lock().unwrap().clone().unwrap();
        flow.cancel();
        tokio::time::timeout(Duration::from_secs(5), async {
            while !task.is_finished() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("the cancel aborts the exchange task");
        assert!(
            lock.try_lock().is_err(),
            "the paused commit still holds the preparation lock"
        );
        release.send(()).unwrap();
        holder.join().unwrap();
        tokio::time::timeout(Duration::from_secs(5), lock.lock_owned())
            .await
            .expect("released once the commit returns");
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// Go `TestSIWCFileMutationRejectsLateExchange`: the exchange has returned and its
    /// commit waits on `disk` while a v8 mutation of the file runs there. The commit
    /// then finds the binding failed and writes nothing: a reupload keeps its bytes,
    /// and a deleted file stays gone. The deletes run the route's own code under the
    /// lock; the upload does what `store_file` does there.
    ///
    /// Single-threaded, as in the test above: once the preparation lock is taken, the
    /// commit is running on the blocking pool, paused on `disk`.
    #[tokio::test]
    async fn a_late_exchange_lands_nothing_after_a_v8_mutation() {
        let app = axum::Router::new().route(
            "/api/accounts/oauth/token",
            axum::routing::post(|| async {
                r#"{"access_token":"siwc-at","refresh_token":"siwc-rt","id_token":"siwc-id","scope":"openid chatgpt.tokens.use.direct","expires_in":3600}"#
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        for (operation, query, body) in [
            ("upload", "", ""),
            ("delete", "name=codex.json", ""),
            ("delete-all", "all=true", ""),
            ("delete-batch", "", r#"["codex.json","other.json"]"#),
        ] {
            let (dir, file, state) = fixture(Some(base.clone()));
            std::fs::write(dir.join("other.json"), codex_file("other")).unwrap();
            let sid = started_state(
                bind(
                    State(state.clone()),
                    FirstJson(axum::body::Bytes::from(r#"{"name":"codex.json"}"#)),
                )
                .await,
            )
            .await;
            let reupload = codex_file("reuploaded");
            let (held_tx, held) = std::sync::mpsc::channel();
            let (go, go_rx) = std::sync::mpsc::channel::<()>();
            let mutation = std::thread::spawn({
                let (state, file, reupload) = (state.clone(), file.clone(), reupload.clone());
                move || {
                    let _disk = state.disk.lock().unwrap();
                    held_tx.send(()).unwrap();
                    go_rx.recv().unwrap();
                    if operation == "upload" {
                        state.oauth.invalidate_siwc(&file);
                        std::fs::write(&file, reupload).unwrap();
                    } else {
                        let q = Query::parse(Some(query.to_owned()));
                        let response = super::super::auth_files::delete_sync(&state, &q, body.as_bytes(), true);
                        assert_eq!(response.status(), StatusCode::OK, "{operation}");
                    }
                }
            });
            held.recv().unwrap();
            let req = CallbackRequest {
                state: sid.clone(),
                code: "c".into(),
                client_id: "oaiapp_issued".into(),
                ..Default::default()
            };
            assert_eq!(callback(&state, &req, None).unwrap().status(), StatusCode::OK);
            let lock = state.rt.store().prepare_lock("codex.json");
            tokio::time::timeout(Duration::from_secs(5), async {
                while lock.try_lock().is_ok() {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await
            .expect("the commit waits on disk");
            go.send(()).unwrap();
            mutation.join().unwrap();
            tokio::time::timeout(Duration::from_secs(5), lock.lock_owned())
                .await
                .expect("the commit returns");
            let (_, status, _, _, completed) = state.oauth.details(&sid).unwrap();
            assert_eq!(
                (status.as_str(), completed),
                ("SIWC binding target changed; start a new binding", false),
                "{operation}"
            );
            if operation == "upload" {
                assert_eq!(std::fs::read_to_string(&file).unwrap(), reupload, "{operation}");
            } else {
                assert!(!file.exists(), "{operation}: the deleted file came back");
            }
            std::fs::remove_dir_all(dir).unwrap();
        }
    }

    /// Go `TestSIWCMutationPathAliases`, plus what Go's `os.SameFile` adds: a hard link
    /// is the same file, and a file that is gone matches through its folder's real path.
    #[cfg(unix)]
    #[test]
    fn mutation_paths_match_every_alias_of_the_file() {
        let dir = std::env::temp_dir().join(format!("cpa-siwc-alias-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(dir.join("real")).unwrap();
        let path = dir.join("real").join("auth.json");
        std::fs::write(&path, "{}").unwrap();
        std::os::unix::fs::symlink(&path, dir.join("real").join("alias.json")).unwrap();
        std::fs::hard_link(&path, dir.join("real").join("hard.json")).unwrap();
        std::fs::write(dir.join("real").join("other.json"), "{}").unwrap();
        std::os::unix::fs::symlink(dir.join("real"), dir.join("linked")).unwrap();
        let at = |rel: &str| dir.join(rel);
        assert!(same_path(&at("real/alias.json"), &path));
        assert!(same_path(&at("real/hard.json"), &path));
        assert!(same_path(&at("linked/auth.json"), &path));
        assert!(same_path(&at("linked/gone.json"), &at("real/gone.json")));
        assert!(!same_path(&path, &at("real/other.json")));
        assert!(!same_path(&path, &at("real/gone.json")));
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// With the file and its folder both gone, the binding's canonical path (saved at
    /// start) and a mutation path compare as Go's `Abs` and `Clean` spell them.
    #[cfg(unix)]
    #[test]
    fn a_gone_folder_compares_cleaned_paths() {
        let root = std::fs::canonicalize(std::env::temp_dir())
            .unwrap()
            .join(format!("cpa-siwc-gone-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(root.join("auth")).unwrap();
        std::fs::write(root.join("auth/codex.json"), "{}").unwrap();
        let bound = std::fs::canonicalize(root.join("auth/codex.json")).unwrap();
        std::fs::remove_dir_all(root.join("auth")).unwrap();
        assert!(same_path(&root.join("auth/../auth/codex.json"), &bound));
        assert!(same_path(&root.join("auth/./codex.json"), &bound));
        assert!(!same_path(&root.join("auth/../codex.json"), &bound));
        assert!(!same_path(&root.join("other/../auth/other.json"), &bound));
        std::fs::remove_dir_all(root).unwrap();
    }

    /// Windows: `fs::canonicalize` gives the verbatim `\\?\` spelling; once the folder
    /// is gone, the ordinary spelling of the same path still matches, case-insensitively.
    #[cfg(windows)]
    #[test]
    fn a_gone_folder_matches_the_verbatim_binding_path() {
        let root = std::env::temp_dir().join(format!("cpa-siwc-gone-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(root.join("auth")).unwrap();
        std::fs::write(root.join(r"auth\codex.json"), "{}").unwrap();
        let bound = std::fs::canonicalize(root.join(r"auth\codex.json")).unwrap();
        let ordinary = PathBuf::from(bound.to_str().unwrap().strip_prefix(r"\\?\").unwrap());
        std::fs::remove_dir_all(root.join("auth")).unwrap();
        assert!(same_path(&ordinary, &bound));
        assert!(!same_path(&ordinary.with_file_name("other.json"), &bound));
        assert!(same_path(
            &PathBuf::from(ordinary.to_str().unwrap().replace(r"\auth\", r"\auth\.\..\AUTH\")),
            &bound
        ));
        assert_eq!(
            clean(Path::new(r"\\?\UNC\server\share\dir\f.json")),
            PathBuf::from(r"\\server\share\dir\f.json")
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    /// Go `BindCodexSIWC` reads, merges and writes the file under its credential lock:
    /// a commit waits while another process's refresh holds it, then binds onto the
    /// file that refresh wrote, so the rotated outer refresh token is kept.
    #[tokio::test]
    async fn a_binding_waits_for_the_credential_lock_and_keeps_a_rotated_token() {
        let app = axum::Router::new().route(
            "/api/accounts/oauth/token",
            axum::routing::post(|| async {
                r#"{"access_token":"siwc-at","refresh_token":"siwc-rt","id_token":"siwc-id","scope":"openid chatgpt.tokens.use.direct","expires_in":3600}"#
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let (dir, file, state) = fixture(Some(base));
        let response = bind(
            State(state.clone()),
            FirstJson(axum::body::Bytes::from(r#"{"name":"codex.json"}"#)),
        )
        .await;
        let sid = started_state(response).await;
        let held = cpa_exec::codex_siwc_bind::lock_file(&file).unwrap();
        let req = CallbackRequest {
            state: sid,
            code: "c".into(),
            client_id: "oaiapp_issued".into(),
            ..Default::default()
        };
        assert_eq!(callback(&state, &req, None).unwrap().status(), StatusCode::OK);
        crate::runtime::tests::lock_waited(&file).await;
        // The other process's refresh spends "r" and writes "r2" under the lock.
        let rotated = codex_file("old").replace(r#""refresh_token":"r""#, r#""refresh_token":"r2""#);
        std::fs::write(&file, rotated).unwrap();
        drop(held);
        let bound = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let disk: Value = serde_json::from_slice(&std::fs::read(&file).unwrap()).unwrap();
                if disk.get("siwc").is_some() {
                    return disk;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("the binding lands once the lock is free");
        assert_eq!(
            (&bound["refresh_token"], &bound["siwc"]["refresh_token"]),
            (&Value::from("r2"), &Value::from("siwc-rt"))
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    fn codex_file(account: &str) -> String {
        format!(r#"{{"type":"codex","account_id":"{account}","access_token":"a","refresh_token":"r"}}"#)
    }

    async fn started_state(response: Response) -> String {
        let body = axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap();
        serde_json::from_slice::<Value>(&body).unwrap()["state"]
            .as_str()
            .unwrap()
            .to_owned()
    }

    /// A private auth dir holding `codex.json` for account "old".
    fn fixture(login_base: Option<String>) -> (PathBuf, PathBuf, Arc<Management>) {
        let dir = std::env::temp_dir().join(format!("cpa-siwc-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("codex.json");
        std::fs::write(&file, codex_file("old")).unwrap();
        let path = dir.join("config.yaml");
        std::fs::write(&path, format!("oauth: {{auth-dir: {}}}\n", dir.display())).unwrap();
        let cfg = cpa_core::config::Config::load(&path).unwrap();
        let rt = Arc::new(crate::testing::runtime(
            cfg.clone(),
            cpa_core::config::credentials::from_auth_dir(&cfg),
            cpa_exec::Executors {
                claude: cpa_exec::claude::ClaudeExecutor::new("http://127.0.0.1:9").unwrap(),
                codex: Default::default(),
                devices: Default::default(),
                openai: Default::default(),
                google: Default::default(),
            },
        ));
        let options = crate::management::Options {
            login_base,
            ..Default::default()
        };
        (dir, file, Management::with_options(rt, path, options))
    }
}
