//! `-siwc-bind` (Go internal/cmd/siwc_bind.go, 7b0319fb): runs the Sign in with
//! ChatGPT browser flow from the command line and writes the `siwc` token partition and
//! `use_siwc` into an existing Codex OAuth credential file in `auth-dir`, under Go's
//! credential lock file (internal/auth/codex/token.go `WithFileLock`).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use cpa_common::json::GoValue;
use cpa_core::config::Config;
use cpa_core::gostr::GoStr;

use crate::claude_login::{Interaction, parse_callback_input};
use crate::codex_oauth::{CodexOAuth, LoginOptions};
use crate::codex_siwc::Tokens;

/// Go `SIWCCallbackPort`, unless `-oauth-callback-port` is set.
pub const CALLBACK_PORT: u16 = 1455;

/// How `-siwc-bind` failed, as Go reports it.
#[derive(Debug, PartialEq, Eq)]
pub enum BindError {
    /// Go `log.Errorf`, before the browser flow.
    Log(String),
    /// Go `fmt.Printf`, once the flow started: the whole line.
    Print(String),
    /// The callback port cannot be bound: Go logs `ErrPortInUse`'s message and exits
    /// with 13.
    PortInUse,
}

type GoMap = BTreeMap<String, GoValue>;

/// Go `json.Unmarshal(data, &map[string]any)`: `Ok(None)` for `null` (a nil map),
/// numbers through float64. The error is the cause Go wraps.
fn decode(data: &[u8]) -> Result<Option<GoMap>, String> {
    match GoValue::parse_f64(data).map(GoValue::into_object) {
        Some(Ok(map)) => Ok(Some(map)),
        Some(Err(GoValue::Null)) => Ok(None),
        Some(Err(_)) => Err("json: cannot unmarshal into Go value of type map[string]interface {}".into()),
        None => Err("invalid JSON".into()),
    }
}

fn go_str<'a>(map: Option<&'a GoMap>, key: &str) -> &'a str {
    match map.and_then(|m| m.get(key)) {
        Some(GoValue::String(s)) => s,
        _ => "",
    }
}

/// Go `validateSIWCBindCredential`.
fn validate(meta: Option<&GoMap>) -> Result<(), String> {
    if !go_str(meta, "type").trim().go_eq_fold("codex") {
        return Err("siwc-bind: credential type must be codex".into());
    }
    for key in ["access_token", "refresh_token"] {
        if go_str(meta, key).trim().is_empty() {
            return Err(format!("siwc-bind: codex credential requires a non-empty {key}"));
        }
    }
    Ok(())
}

/// Go `ResolveSIWCBindPath`: an absolute path or a bare file name in `auth_dir`, whose
/// real path (symlinks resolved) is a regular Codex OAuth file inside the real
/// `auth_dir`. Returns that real path.
pub fn resolve(auth_dir: &Path, target: &str) -> Result<PathBuf, String> {
    let raw = target.trim();
    if raw.is_empty() {
        return Err("siwc-bind: missing codex credential file path".into());
    }
    if auth_dir.to_string_lossy().trim().is_empty() {
        return Err("siwc-bind: auth-dir is not configured".into());
    }
    // Go `util.ResolveAuthDir` cleans lexically and `filepath.Abs` joins the working
    // directory before `EvalSymlinks`: `link/../auth` is a sibling of `link`, never the
    // parent of its target.
    let dir = go_abs(auth_dir)
        .and_then(std::fs::canonicalize)
        .map(plain_path)
        .map_err(|e| format!("siwc-bind: failed to resolve real auth-dir: {e}"))?;
    let candidate = if Path::new(raw).is_absolute() {
        PathBuf::from(raw)
    } else if raw.contains(['/', '\\']) || raw == "." || raw == ".." {
        return Err("siwc-bind: expected an absolute path or bare filename".into());
    } else {
        dir.join(raw)
    };
    let candidate = std::fs::canonicalize(candidate)
        .map(plain_path)
        .map_err(|e| format!("siwc-bind: failed to resolve credential file: {e}"))?;
    if candidate
        .strip_prefix(&dir)
        .map_or(true, |rel| rel.as_os_str().is_empty())
    {
        return Err("siwc-bind: credential file must belong to auth-dir".into());
    }
    if !std::fs::metadata(&candidate).is_ok_and(|m| m.is_file()) {
        return Err("siwc-bind: credential target must be an existing regular file".into());
    }
    let data = std::fs::read(&candidate)
        .map_err(|e| format!("siwc-bind: failed to read credential file {}: {e}", candidate.display()))?;
    let meta = decode(&data).map_err(|_| format!("siwc-bind: invalid credential JSON in {}", candidate.display()))?;
    validate(meta.as_ref())?;
    Ok(candidate)
}

/// Go `ensureSIWCDeviceIDFromFile`: the file's valid `siwc.device_id`, lower-cased, else a
/// new UUID.
pub fn device_id(path: &Path) -> Result<String, String> {
    let data = std::fs::read(path).map_err(|e| format!("failed to read credential file: {e}"))?;
    let meta = decode(&data).map_err(|e| format!("invalid credential JSON: {e}"))?;
    if let Some(GoValue::Object(siwc)) = meta.as_ref().and_then(|m| m.get("siwc"))
        && let Some(GoValue::String(id)) = siwc.get("device_id")
        && crate::codex_siwc::is_valid_uuid(id)
    {
        return Ok(id.trim().go_lower());
    }
    Ok(uuid::Uuid::new_v4().to_string())
}

/// Go `saveSIWCToCodexCredentialFile` after its log line: under the credential lock, the
/// current file with `use_siwc` set and `siwc` replaced, encoded as Go's `json.Marshal`
/// of the decoded map (sorted keys, numbers through float64), written atomically.
/// Blocking.
pub fn save(path: &Path, tokens: &Tokens) -> Result<(), String> {
    with_file_lock(path, || {
        let data = std::fs::read(path).map_err(|e| format!("failed to read existing credential file: {e}"))?;
        let meta = decode(&data).map_err(|e| format!("failed to parse existing credential file: {e}"))?;
        validate(meta.as_ref())?;
        let mut meta = meta.unwrap_or_default();
        // Binding is an explicit SIWC replacement, not a stale snapshot merge.
        meta.insert("use_siwc".into(), GoValue::Bool(true));
        meta.insert(
            "siwc".into(),
            GoValue::from_json(&serde_json::Value::Object(tokens.to_map())),
        );
        let bytes = GoValue::Object(meta).marshal();
        let dir = path.parent().unwrap_or(Path::new("."));
        crate::codex_oauth::publish(dir, path, &bytes).map_err(|e| format!("failed to replace credential file: {e}"))
    })
}

/// Go `WithFileLock`'s inter-process half: an exclusive lock on `<path>.lock` for the
/// duration of `f` (`flock`, or `LockFileEx` on Windows), so a Go or Rust process
/// refreshing the same file waits.
// ponytail: Go also serializes callers in one process with a mutex per path; here the
// server's refresh holds its per-credential preparation lock around [`lock_file`].
pub fn with_file_lock<T>(path: &Path, f: impl FnOnce() -> Result<T, String>) -> Result<T, String> {
    let file = lock_file(path)?;
    let result = f();
    drop(file);
    result
}

/// The lock of [`with_file_lock`], held until the returned file is dropped (closing it
/// releases the lock). Blocking.
pub fn lock_file(path: &Path) -> Result<std::fs::File, String> {
    let (file, lock_path) = open_lock(path)?;
    lock(&file, true).map_err(|e| format!("failed to acquire flock on {}: {e}", lock_path.display()))?;
    Ok(file)
}

/// The lock file of [`lock_file`], open but not yet locked; [`try_lock`] takes it.
pub fn open_lock_file(path: &Path) -> Result<std::fs::File, String> {
    open_lock(path).map(|(file, _)| file)
}

/// Takes the lock on a file [`open_lock_file`] opened, if no one holds it: `false`
/// when another holder has it. Never blocks.
pub fn try_lock(file: &std::fs::File) -> Result<bool, String> {
    match lock(file, false) {
        Ok(()) => Ok(true),
        Err(e) if would_block(&e) => Ok(false),
        Err(e) => Err(format!("failed to acquire credential lock: {e}")),
    }
}

fn open_lock(path: &Path) -> Result<(std::fs::File, PathBuf), String> {
    let mut lock_path = lock_base(path).into_os_string();
    lock_path.push(".lock");
    let lock_path = PathBuf::from(lock_path);
    let mut options = std::fs::OpenOptions::new();
    options.read(true).write(true).create(true).truncate(false);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    let file = options
        .open(&lock_path)
        .map_err(|e| format!("failed to open credential lock file {}: {e}", lock_path.display()))?;
    Ok((file, lock_path))
}

#[cfg(unix)]
fn lock(file: &std::fs::File, wait: bool) -> std::io::Result<()> {
    use std::os::fd::AsRawFd;
    let operation = if wait {
        libc::LOCK_EX
    } else {
        libc::LOCK_EX | libc::LOCK_NB
    };
    loop {
        // SAFETY: a valid open descriptor; flock touches no memory.
        if unsafe { libc::flock(file.as_raw_fd(), operation) } == 0 {
            return Ok(());
        }
        let error = std::io::Error::last_os_error();
        if error.kind() != std::io::ErrorKind::Interrupted {
            return Err(error);
        }
    }
}

#[cfg(unix)]
fn would_block(error: &std::io::Error) -> bool {
    error.kind() == std::io::ErrorKind::WouldBlock
}

#[cfg(windows)]
fn lock(file: &std::fs::File, wait: bool) -> std::io::Result<()> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::{LOCKFILE_EXCLUSIVE_LOCK, LOCKFILE_FAIL_IMMEDIATELY, LockFileEx};
    let flags = if wait {
        LOCKFILE_EXCLUSIVE_LOCK
    } else {
        LOCKFILE_EXCLUSIVE_LOCK | LOCKFILE_FAIL_IMMEDIATELY
    };
    // Go locks the first byte; overlapping ranges conflict, so the two interoperate.
    // SAFETY: a valid open handle and a zeroed OVERLAPPED that outlives the call.
    let ok = unsafe {
        let mut overlapped: windows_sys::Win32::System::IO::OVERLAPPED = std::mem::zeroed();
        LockFileEx(file.as_raw_handle() as _, flags, 0, 1, 0, &mut overlapped)
    };
    if ok == 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(windows)]
fn would_block(error: &std::io::Error) -> bool {
    // ERROR_LOCK_VIOLATION
    error.raw_os_error() == Some(33)
}

#[cfg(not(any(unix, windows)))]
fn lock(_: &std::fs::File, _: bool) -> std::io::Result<()> {
    Ok(())
}

#[cfg(not(any(unix, windows)))]
fn would_block(_: &std::io::Error) -> bool {
    false
}

/// Go `filepath.Clean`, lexically.
fn go_clean(path: &Path) -> PathBuf {
    use std::path::Component;
    let mut out: Vec<Component<'_>> = Vec::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => match out.last() {
                Some(Component::Normal(_)) => {
                    out.pop();
                }
                Some(Component::RootDir | Component::Prefix(_)) => {}
                _ => out.push(component),
            },
            other => out.push(other),
        }
    }
    if out.is_empty() {
        return PathBuf::from(".");
    }
    out.iter().collect()
}

/// A canonical path spelled as Go's `filepath.EvalSymlinks` spells it. On Windows,
/// `std::fs::canonicalize` answers with a verbatim path: `\\?\C:\x` becomes `C:\x`
/// and `\\?\UNC\server\share\x` becomes `\\server\share\x`; other verbatim
/// forms stay. Other platforms return the path as it is.
#[cfg(windows)]
pub fn plain_path(path: PathBuf) -> PathBuf {
    use std::os::windows::ffi::{OsStrExt, OsStringExt};
    use std::path::{Component, Prefix};
    let wide: Vec<u16> = path.as_os_str().encode_wide().collect();
    let plain = match path.components().next() {
        Some(Component::Prefix(p)) => match p.kind() {
            // `\\?\` is four UTF-16 units, `\\?\UNC\` eight.
            Prefix::VerbatimDisk(_) => wide[4..].to_vec(),
            Prefix::VerbatimUNC(..) => [&[u16::from(b'\\'); 2][..], &wide[8..]].concat(),
            _ => return path,
        },
        _ => return path,
    };
    PathBuf::from(std::ffi::OsString::from_wide(&plain))
}

/// The path as it is: only Windows spells canonical paths verbatim.
#[cfg(not(windows))]
pub fn plain_path(path: PathBuf) -> PathBuf {
    path
}

/// Go `filepath.Abs` of a cleaned path: joined with the working directory, cleaned.
fn go_abs(path: &Path) -> std::io::Result<PathBuf> {
    std::path::absolute(go_clean(path)).map(|p| go_clean(&p))
}

/// Go `canonicalLockPath`: the path trimmed of surrounding white space, cleaned and made
/// absolute. The lock file is this plus `.lock`, so a file named with trailing spaces
/// shares its lock with Go's.
fn lock_base(path: &Path) -> PathBuf {
    let trimmed = path.to_str().map_or(path, |s| Path::new(s.trim()));
    go_abs(trimmed).unwrap_or_else(|_| go_clean(trimmed))
}

/// `-siwc-bind <file>`: the browser flow on the callback port (`-oauth-callback-port`,
/// else 1455), then [`save`]. Returns the bound file.
pub async fn bind(cfg: &Config, target: &str, options: &LoginOptions) -> Result<PathBuf, BindError> {
    let port = match options.callback_port {
        0 => CALLBACK_PORT,
        port => port,
    };
    let no_browser = options.no_browser;
    let interaction = Interaction {
        manual_delay: Duration::from_secs(15),
        prompt: Some(Box::new(|| {
            Box::pin(async {
                use std::io::Write;
                print!("Paste the callback URL (or press Enter to keep waiting): ");
                let _ = std::io::stdout().flush();
                // misc.AsyncPrompt: a detached reader never keeps the process alive.
                let (tx, rx) = tokio::sync::oneshot::channel();
                std::thread::spawn(move || {
                    let mut line = String::new();
                    let _ = tx.send(std::io::stdin().read_line(&mut line).map(|_| line));
                });
                rx.await.map_err(std::io::Error::other)?
            })
        })),
        show_url: Box::new(move |url| show_url(url, port, no_browser)),
    };
    // Go `NewCodexAuth(cfg)`: `requests.proxy-url` on a standard client.
    let proxy = cfg
        .document
        .get("requests")
        .and_then(|r| r.get("proxy-url"))
        .and_then(|v| v.as_str())
        .unwrap_or_default();
    let client = crate::proxy::GoClients::new(Default::default()).get(&crate::proxy::Proxy::parse(proxy));
    bind_with(cfg, target, port, CodexOAuth::new(client), interaction).await
}

fn show_url(url: &str, port: u16, no_browser: bool) {
    if !no_browser {
        println!("Opening browser for OpenAI Sign in with ChatGPT authentication");
        if crate::claude_login::open_browser(url) {
            println!("Waiting for Sign in with ChatGPT authentication callback...");
            return;
        }
        tracing::warn!("No browser available; please open the URL manually");
    }
    crate::claude_login::print_ssh_tunnel_instructions(port);
    println!("Visit the following URL to continue authentication:\n{url}");
    println!("Waiting for Sign in with ChatGPT authentication callback...");
}

/// [`bind`] through `oauth` and `interaction` (tests use local endpoints).
pub(crate) async fn bind_with(
    cfg: &Config,
    target: &str,
    port: u16,
    oauth: CodexOAuth,
    interaction: Interaction,
) -> Result<PathBuf, BindError> {
    let path = resolve(&cfg.auth_dir, target).map_err(BindError::Log)?;
    let device = {
        let path = path.clone();
        tokio::task::spawn_blocking(move || device_id(&path))
            .await
            .map_err(|e| BindError::Log(format!("siwc-bind: {e}")))?
            .map_err(|e| BindError::Log(format!("siwc-bind: {e}")))?
    };
    let redirect = format!("http://127.0.0.1:{port}/auth/callback");
    let (verifier, challenge) =
        crate::oauth::pkce().map_err(|_| BindError::Log("siwc-bind: PKCE generation failed".into()))?;
    let random = |what: &str| {
        crate::codex_siwc::random_base64url()
            .map_err(|_| BindError::Log(format!("siwc-bind: {what} generation failed")))
    };
    let (state, nonce) = (random("state")?, random("nonce")?);
    // Go reports any failure to listen as the port being in use. The redirect is
    // loopback, so loopback is all that has to listen (Go takes every interface).
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", port))
        .await
        .map_err(|_| BindError::PortInUse)?;
    let identifier = crate::codex_siwc::identifier(cfg);
    let url =
        crate::codex_siwc::authorize_url(&state, &nonce, &device, &redirect, &challenge, &identifier).map_err(|e| {
            BindError::Log(format!(
                "siwc-bind: failed to generate authorization URL: {}",
                String::from_utf8_lossy(&e.body)
            ))
        })?;
    // Go's callback server runs until the command returns, so the browser's redirect
    // to /success is answered; the first callback wins.
    let (results, mut callbacks) = tokio::sync::mpsc::channel(1);
    let server = tokio::spawn(async move {
        while let Ok(callback) = crate::codex_oauth::callback(&listener).await {
            let _ = results.try_send(callback);
        }
    });
    (interaction.show_url)(&url);
    let result = async {
        let failed = |e: String| BindError::Print(format!("Sign in with ChatGPT authentication failed: {e}"));
        let (code, got_state, client_id, error, description) =
            wait(&mut callbacks, interaction.manual_delay, interaction.prompt)
                .await
                .map_err(failed)?;
        if !error.is_empty() {
            // Go `OAuthError.Error`.
            return Err(failed(if description.is_empty() {
                format!("OAuth error: {error}")
            } else {
                format!("OAuth error {error}: {description}")
            }));
        }
        if got_state != state {
            return Err(failed("state mismatch".into()));
        }
        if client_id.trim().is_empty() {
            return Err(failed("callback did not contain an issued client ID".into()));
        }
        let tokens = oauth
            .exchange_siwc(&code, &client_id, &redirect, &device, &verifier)
            .await
            .map_err(|e| {
                BindError::Print(format!(
                    "Sign in with ChatGPT token exchange failed: {}",
                    String::from_utf8_lossy(&e.body)
                ))
            })?;
        // Go `misc.LogSavingCredentials`.
        println!("Saving credentials to {}", path.display());
        let target = path.clone();
        tokio::task::spawn_blocking(move || save(&target, &tokens))
            .await
            .map_err(|e| e.to_string())
            .and_then(|r| r)
            .map_err(|e| BindError::Print(format!("Failed to save SIWC credentials: {e}")))?;
        Ok(path)
    }
    .await;
    // Give the browser's /success a moment before the listener closes.
    tokio::time::sleep(Duration::from_millis(200)).await;
    server.abort();
    result
}

/// The callback (code, state, client ID, error, description): the local server's
/// first, a pasted URL after `delay`, or Go's 5-minute timeout. An empty paste keeps
/// waiting without asking again.
async fn wait(
    callbacks: &mut tokio::sync::mpsc::Receiver<crate::codex_oauth::Callback>,
    delay: Duration,
    prompt: Option<crate::claude_login::Prompt>,
) -> Result<(String, String, String, String, String), String> {
    let deadline = tokio::time::sleep(Duration::from_secs(300));
    tokio::pin!(deadline);
    let manual = async {
        let prompt = prompt?;
        tokio::time::sleep(delay).await;
        Some(prompt().await)
    };
    tokio::pin!(manual);
    let mut manual_done = false;
    loop {
        tokio::select! {
            biased;
            Some(c) = callbacks.recv() => {
                return Ok((c.code, c.state, c.client_id, c.error.unwrap_or_default(), String::new()));
            }
            () = &mut deadline => return Err("timeout waiting for OAuth callback".into()),
            input = &mut manual, if !manual_done => {
                manual_done = true;
                let Some(input) = input else { continue };
                let input = input.map_err(|e| e.to_string())?;
                if let Some(c) = parse_callback_input(&input)? {
                    return Ok((c.code, c.state, c.client_id, c.error, c.description));
                }
            }
        }
    }
}

#[cfg(test)]
#[path = "codex_siwc_bind_tests.rs"]
mod tests;
